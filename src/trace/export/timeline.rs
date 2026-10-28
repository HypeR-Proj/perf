// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Scheduling windows have observed endpoints, not invented capture boundaries.
//! Unlocated producer loss cannot prove continuity even when identities match.

use super::{Capture, Event, TraceWriter, fields, scheduling};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
};

pub(super) struct Run<'a> {
    pub cpu: usize,
    pub thread: u64,
    pub begin: &'a Event,
    pub end: &'a Event,
    conflict: bool,
}

/// Emit non-overlapping CPU lanes and separate per-thread execution lanes.
pub(super) fn write(
    c: &Capture,
    threads: &BTreeMap<u64, u32>,
    w: &mut TraceWriter<impl Write>,
) -> io::Result<()> {
    let runs = runs(c);
    w.name(5, 0, "HypeR thread execution")?;
    let mut named = BTreeSet::new();
    for run in runs {
        let name = c
            .names
            .get(&run.thread)
            .cloned()
            .unwrap_or_else(|| format!("thread {:#x}", run.thread));
        let cause = fields(c, run.end);
        let args = json!({"thread":format!("{:#x}",run.thread),"cpu":run.cpu,
            "class":scheduling::next_class(c,run.begin),
            "end_state":scheduling::previous_state(c,run.end),
            "switch_out_reason":cause["switch_reason"],"preempted":cause["preempted"],
            "schedule_point":cause["schedule_point"],
            "switch_in_sequence":run.begin.sequence,"switch_out_sequence":run.end.sequence});
        w.span(
            (1, run.cpu as u32 + 1),
            &name,
            "scheduler",
            run.begin.time..run.end.time,
            args.clone(),
        )?;
        if let Some(&tid) = threads.get(&run.thread) {
            if named.insert(run.thread) {
                w.name(5, tid, &format!("{name} [{:#x}]", run.thread))?;
            }
            let state = if scheduling::next_class(c, run.begin) == Some(3) {
                "Idle"
            } else {
                "Running"
            };
            w.span(
                (5, tid),
                state,
                "scheduler",
                run.begin.time..run.end.time,
                args,
            )?;
        }
    }
    Ok(())
}

/// Bounded observations shared by the JSON and native scheduler exporters.
pub(super) fn runs(c: &Capture) -> Vec<Run<'_>> {
    let count = c.info.cpu_count as usize;
    let mut previous = vec![None; count];
    let mut running: Vec<Option<(&Event, u64)>> = vec![None; count];
    let mut runs = Vec::new();
    for e in &c.events {
        if previous[e.cpu].is_some_and(|n: u64| n.checked_add(1) != Some(e.sequence)) {
            running[e.cpu] = None;
        }
        previous[e.cpu] = Some(e.sequence);
        if c.sites[e.site].name != "sched_switch" {
            continue;
        }
        let (Some(prev), Some(next)) = (c.value(e, "prev"), c.value(e, "next")) else {
            running[e.cpu] = None;
            continue;
        };
        if let Some((begin, thread)) = running[e.cpu]
            && thread == prev
            && begin.time < e.time
        {
            runs.push(Run {
                cpu: e.cpu,
                thread,
                begin,
                end: e,
                conflict: false,
            });
        }
        running[e.cpu] = Some((e, next));
    }

    // Missing migration boundaries can otherwise produce crossing slices for
    // one Thread on different CPUs. Omit conflicting execution windows rather
    // than asking the viewer to invent a nesting/order for them.
    let mut order: Vec<_> = (0..runs.len()).collect();
    order.sort_unstable_by_key(|&i| (runs[i].thread, runs[i].begin.time, runs[i].end.time));
    let mut first = 0;
    while first < order.len() {
        let mut last = first + 1;
        let mut end = runs[order[first]].end.time;
        while last < order.len()
            && runs[order[last]].thread == runs[order[first]].thread
            && runs[order[last]].begin.time < end
        {
            end = end.max(runs[order[last]].end.time);
            last += 1;
        }
        if last - first > 1 {
            for &i in &order[first..last] {
                runs[i].conflict = true;
            }
        }
        first = last;
    }

    runs.retain(|run| !run.conflict);
    runs
}
