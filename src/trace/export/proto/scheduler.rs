// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Translate observed `HypeR` states to Perfetto's OS-independent scheduler model.
//! No invented switch-in at capture start or switch-out at the deadline.

use super::{Message, Output, track};
use crate::export::{Capture, fields, scheduling, timeline};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

pub(super) fn write(c: &Capture, threads: &BTreeMap<u64, u32>, out: &mut Output) -> io::Result<()> {
    let runs = timeline::runs(c);
    let mut dispatches = BTreeMap::<u64, Vec<_>>::new();
    let mut reaped = BTreeMap::new();
    let mut gaps = Vec::new();
    let mut sequences = vec![None; c.info.cpu_count as usize];
    for event in &c.events {
        if sequences[event.cpu]
            .replace(event.sequence)
            .is_some_and(|s: u64| s.checked_add(1) != Some(event.sequence))
        {
            gaps.push(event.time);
        }
        if c.sites[event.site].name == "sched_switch"
            && let Some(next) = c.value(event, "next")
        {
            dispatches.entry(next).or_default().push(event);
        }
        if c.sites[event.site].name == "sched_reap"
            && let Some(thread) = c.value(event, "thread")
        {
            reaped.insert(thread, event);
        }
    }
    let mut per_thread = BTreeMap::<u64, Vec<usize>>::new();
    for (index, run) in runs.iter().enumerate() {
        let (Some(&tid), Some(end_state)) = (
            threads.get(&run.thread),
            end_state(scheduling::previous_state(c, run.end)),
        ) else {
            continue;
        };
        per_thread.entry(run.thread).or_default().push(index);
        state(out, c, run.begin.time, run.cpu, tid, 3)?;
        let end_state = if end_state == 7
            && reaped
                .get(&run.thread)
                .is_some_and(|event| event.time == run.end.time)
        {
            // Perfetto rejects two state changes at the same timestamp.
            // A zero-length Zombie interval collapses to Destroyed, on the
            // last running CPU so this also closes the pending sched slice.
            reaped.remove(&run.thread);
            8
        } else {
            end_state
        };
        state(out, c, run.end.time, run.cpu, tid, end_state)?;
        // GenericKernel carries state; retain HypeR's richer switch reason and
        // entry-point facts on the same real Thread's activity track.
        out.event(
            json!({"ph":"i","pid":2,"tid":tid,"name":"Switch out","cat":"scheduler",
            "ts":run.end.time.saturating_sub(c.info.started_ns) as f64/1000.0,"args":fields(c,run.end)}),
        )?;
    }
    for (thread, event) in reaped {
        if let Some(&tid) = threads.get(&thread) {
            // Reaping remains valid even if the final run was filtered or lost.
            state(out, c, event.time, event.cpu, tid, 8)?;
        }
    }
    let mut seen = BTreeSet::new();
    let mut flow = 1u64 << 63;
    for event in &c.events {
        if c.sites[event.site].name != "sched_wakeup" {
            continue;
        }
        let Some(thread) = c.value(event, "thread") else {
            continue;
        };
        let Some(indices) = per_thread.get(&thread) else {
            continue;
        };
        let position = indices.partition_point(|&i| runs[i].begin.time < event.time);
        if position == 0 || position == indices.len() {
            continue;
        }
        let previous = &runs[indices[position - 1]];
        let next = &runs[indices[position]];
        if scheduling::previous_state(c, previous.end) != Some(4) || event.time < previous.end.time
        {
            continue;
        }
        // A filtered/missing dispatch is not evidence that the thread waited
        // until the next drawable run. Do not bridge a known hole or skip an
        // earlier observed dispatch when calculating wakeup latency.
        let gap = gaps.partition_point(|time| *time < previous.end.time);
        if gaps.get(gap).is_some_and(|time| *time <= next.begin.time) {
            continue;
        }
        let Some(dispatches) = dispatches.get(&thread) else {
            continue;
        };
        let first = dispatches.partition_point(|e| e.time < event.time);
        if !dispatches
            .get(first)
            .is_some_and(|e| e.cpu == next.begin.cpu && e.sequence == next.begin.sequence)
            || !seen.insert((thread, previous.end.time))
        {
            continue;
        }
        let Some(&tid) = threads.get(&thread) else {
            continue;
        };
        state(out, c, event.time, next.cpu, tid, 2)?;
        if let Some(from) = c
            .value(event, "waker")
            .filter(|id| *id != 0)
            .and_then(|id| threads.get(&id))
        {
            flow += 1;
            for (time, tid, outgoing, name) in [
                (event.time, *from, true, "Wakeup"),
                (next.begin.time, tid, false, "Scheduled"),
            ] {
                let mut e = Message::default();
                e.number(9, 3);
                e.number(11, track(2, tid));
                e.text(23, name);
                e.text(22, "scheduler");
                e.fixed(if outgoing { 47 } else { 48 }, flow);
                let mut args = fields(c, event);
                args["latency_ns"] = json!(next.begin.time - event.time);
                super::annotations(&mut e, &args);
                out.push(time.saturating_sub(c.info.started_ns), 3, 11, e)?;
            }
        }
    }
    Ok(())
}

fn end_state(state: Option<u64>) -> Option<u64> {
    match state {
        Some(1 | 3 | 5) => Some(2), // Ready, idle or migrating remains runnable.
        Some(4) => Some(4),         // Blocked: generic sleep, without Linux signal semantics.
        Some(6) => Some(7),         // Terminated.
        _ => None,
    }
}

fn state(
    out: &mut Output,
    c: &Capture,
    time: u64,
    cpu: usize,
    tid: u32,
    state: u64,
) -> io::Result<()> {
    let mut event = Message::default();
    event.number(1, cpu as u64);
    if let Some(name) = out.names.get(&tid) {
        event.text(2, name);
    }
    event.number(3, out.tids[&tid]);
    event.number(4, state);
    // prio is deliberately absent: scheduling class is not a priority value.
    out.push(
        time.saturating_sub(c.info.started_ns),
        match state {
            3 => 2,
            // A coarse clock can give exit and reap the same timestamp.
            // Finish all scheduling transitions before ending the identity.
            8 => 4,
            _ => 1,
        },
        117,
        event,
    )
}
