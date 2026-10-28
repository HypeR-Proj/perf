// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Logical batches and physical commands have different lifetimes and cardinality.
//! Index committed identities before rendering so equal-time cross-CPU events
//! never depend on the arbitrary CPU tie-break used by the raw event stream.

use super::{Capture, Event, TraceWriter, fields};
use serde_json::json;
use std::{collections::BTreeMap, io};

type Key = (u64, u64);
type Track = (u32, u32, &'static str);

#[derive(Default)]
struct Lifetime<'a> {
    begin: Option<&'a Event>,
    detail: Option<&'a Event>,
    end: Option<&'a Event>,
    release: Option<&'a Event>,
    ambiguous: bool,
}

impl<'a> Lifetime<'a> {
    fn add(&mut self, role: usize, event: &'a Event) {
        let field = match role {
            0 => &mut self.begin,
            1 => &mut self.detail,
            2 => &mut self.end,
            _ => &mut self.release,
        };
        self.ambiguous |= field.replace(event).is_some();
    }
    fn interval(&self) -> Option<(&'a Event, &'a Event)> {
        let (begin, end) = (self.begin?, self.end?);
        (!self.ambiguous && begin.time <= end.time).then_some((begin, end))
    }
}

fn lane(
    lanes: &mut BTreeMap<(u64, u64, u64), u32>,
    key: (u64, u64, u64),
    pid: u32,
    w: &mut TraceWriter<impl io::Write>,
) -> io::Result<u32> {
    if let Some(id) = lanes.get(&key) {
        return Ok(*id);
    }
    let id = lanes.len() as u32 + 1;
    let (device, queue, slot) = key;
    let name = if pid == 4 {
        format!("device {device:#x} transport slot {slot}")
    } else {
        format!("device {device:#x} queue {queue:#x} ticket slot {slot}")
    };
    w.name(pid, id, &name)?;
    lanes.insert(key, id);
    Ok(id)
}

fn edge(
    w: &mut TraceWriter<impl io::Write>,
    next: &mut u64,
    from: (Track, &Event),
    to: (Track, &Event),
    name: &str,
    args: serde_json::Value,
) -> io::Result<()> {
    if from.1.time > to.1.time {
        return Ok(());
    }
    // IPC owns the low half of export-local flow IDs. Kernel identities are
    // separately serialized as strings and grant no authority here.
    *next += 1;
    let id = (1u64 << 63) | *next;
    w.anchor_track(from.0, from.1, name, id, true, args.clone())?;
    w.anchor_track(to.0, to.1, name, id, false, args)
}

pub(super) fn export(
    c: &Capture,
    threads: &BTreeMap<u64, u32>,
    w: &mut TraceWriter<impl io::Write>,
) -> io::Result<()> {
    if !c.cross_cpu_spans() {
        return Ok(());
    }
    let mut previous = BTreeMap::<usize, u64>::new();
    let mut batches = BTreeMap::<Key, Lifetime<'_>>::new();
    let mut commands = BTreeMap::<Key, Lifetime<'_>>::new();
    let mut links = BTreeMap::<(u64, u64, u64), (&Event, u64)>::new();
    for e in &c.events {
        if previous
            .insert(e.cpu, e.sequence)
            .is_some_and(|seq| seq.checked_add(1) != Some(e.sequence))
        {
            return Ok(());
        }
        let Some(device) = c.value(e, "domain") else {
            continue;
        };
        let name = c.sites[e.site].name.as_str();
        let role = match name {
            "block_queued" | "block_submit" => 0,
            "block_origin" | "block_command" => 1,
            "block_ready" | "block_complete" | "block_abort" => 2,
            "block_release" => 3,
            "block_link" => {
                if let (Some(tag), Some(request), Some(bytes)) = (
                    c.value(e, "tag"),
                    c.value(e, "request"),
                    c.value(e, "bytes"),
                ) {
                    let entry = links.entry((device, tag, request)).or_insert((e, 0));
                    let Some(total) = entry.1.checked_add(bytes) else {
                        return Ok(());
                    };
                    entry.1 = total;
                }
                continue;
            }
            _ => continue,
        };
        let (table, id) = if matches!(
            name,
            "block_queued" | "block_origin" | "block_ready" | "block_release"
        ) {
            (&mut batches, c.value(e, "request"))
        } else {
            (&mut commands, c.value(e, "tag"))
        };
        if let Some(id) = id {
            table.entry((device, id)).or_default().add(role, e);
        }
    }
    let mut physical_lanes = BTreeMap::new();
    let mut logical_lanes = BTreeMap::new();
    let mut command_tracks = BTreeMap::new();
    let mut batch_tracks = BTreeMap::new();
    let mut depths = BTreeMap::<u64, Vec<(u64, bool)>>::new();
    let mut next_flow = 0;
    for (&(device, tag), command) in &commands {
        let Some((begin, end)) = command.interval() else {
            continue;
        };
        let Some(detail) = command
            .detail
            .filter(|e| begin.time <= e.time && e.time <= end.time)
        else {
            continue;
        };
        let Some(slot) = c.value(detail, "slot") else {
            continue;
        };
        let tid = lane(&mut physical_lanes, (device, 0, slot), 4, w)?;
        let track = (4, tid, "io");
        let name = if c.sites[end.site].name == "block_abort" {
            "aborted command"
        } else {
            match c.value(detail, "opcode") {
                Some(0x88) => "read",
                Some(0x8a) => "write",
                Some(0x91) => "flush",
                _ => "control",
            }
        };
        let mut args = fields(c, begin);
        args["command"] = fields(c, detail);
        args["completion"] = fields(c, end);
        w.span((4, tid), name, "io", begin.time..end.time, args)?;
        command_tracks.insert((device, tag), track);
        depths
            .entry(device)
            .or_default()
            .extend([(begin.time, true), (end.time, false)]);
    }
    for (&key, batch) in &batches {
        let Some((begin, end)) = batch.interval() else {
            continue;
        };
        let (Some(queue), Some(ticket)) = (c.value(begin, "queue"), c.value(begin, "ticket"))
        else {
            continue;
        };
        let tid = lane(&mut logical_lanes, (key.0, queue, ticket & 255), 7, w)?;
        let track = (7, tid, "io");
        let mut args = fields(c, begin);
        args["completion"] = fields(c, end);
        w.span((7, tid), "Block batch", "io", begin.time..end.time, args)?;
        if let Some(release) = batch
            .release
            .filter(|e| end.time <= e.time && c.value(e, "queue") == Some(queue))
        {
            w.span(
                (7, tid),
                "Completion retained",
                "io",
                end.time..release.time,
                fields(c, release),
            )?;
        }
        if let Some(origin) = batch
            .detail
            .filter(|e| begin.time <= e.time && e.time <= end.time)
            && let Some(thread) = c
                .value(origin, "thread")
                .filter(|id| *id != 0)
                .and_then(|id| threads.get(&id))
        {
            // Origin is recorded after queue publication but before unlocking
            // admission. Anchor on that same observation, not a fabricated
            // reverse-time edge to the earlier queued marker.
            edge(
                w,
                &mut next_flow,
                ((2, *thread, "io"), origin),
                (track, origin),
                "Block admission",
                fields(c, origin),
            )?;
        }
        batch_tracks.insert(key, track);
    }
    let mut first_dispatch = BTreeMap::<Key, &Event>::new();
    for ((device, tag, request), (link, bytes)) in links {
        let key = (device, request);
        let (Some(command_track), Some(batch_track)) =
            (command_tracks.get(&(device, tag)), batch_tracks.get(&key))
        else {
            continue;
        };
        let (Some(batch), Some(command)) = (batches.get(&key), commands.get(&(device, tag))) else {
            continue;
        };
        let (Some((queued, ready)), Some((submit, complete))) =
            (batch.interval(), command.interval())
        else {
            continue;
        };
        if !(queued.time <= submit.time
            && submit.time <= link.time
            && link.time <= complete.time
            && complete.time <= ready.time)
        {
            continue;
        }
        first_dispatch
            .entry(key)
            .and_modify(|event| {
                if submit.time < event.time {
                    *event = submit;
                }
            })
            .or_insert(submit);
        let mut args = fields(c, link);
        args["bytes"] = json!(bytes);
        edge(
            w,
            &mut next_flow,
            (*batch_track, queued),
            (*command_track, link),
            "Block dispatch",
            args.clone(),
        )?;
        edge(
            w,
            &mut next_flow,
            (*command_track, complete),
            (*batch_track, ready),
            "Block completion",
            args,
        )?;
    }
    for (key, dispatch) in first_dispatch {
        if let (Some(batch), Some(track)) = (batches.get(&key), batch_tracks.get(&key))
            && let Some(queued) = batch.begin
        {
            w.span(
                (track.0, track.1),
                "Dispatch delay",
                "io",
                queued.time..dispatch.time,
                fields(c, queued),
            )?;
        }
    }
    for (index, (device, mut changes)) in depths.into_iter().enumerate() {
        let tid = index as u32 + 1;
        w.name(6, tid, &format!("device {device:#x}"))?;
        // Starts precede ends at equal timestamps, including zero-duration
        // commands whose complete boundary was read from another CPU first.
        changes.sort_unstable_by_key(|&(time, begin)| (time, !begin));
        let mut count = 0u64;
        for (time, begin) in changes {
            if begin {
                count += 1;
            } else {
                count -= 1;
            }
            w.event(json!({"ph":"C", "pid":6, "tid":tid, "name":format!("Captured commands {device:#x}"),
                "ts":w.time(time), "args":{"in flight":count}}))?;
        }
    }
    Ok(())
}
