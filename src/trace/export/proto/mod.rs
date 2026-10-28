// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Perfetto's stable `GenericKernel` and `TrackEvent` protocols.
//! Schemas: <https://github.com/google/perfetto/tree/v58.2/protos/perfetto/trace>
//! Thread IDs preserve full observed KOIDs; compact track UUIDs and synthetic
//! process IDs are allocated separately from those diagnostic identities.

mod scheduler;
mod wire;

use super::{Capture, metadata};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{self, Write},
};
use wire::Message;

struct Packet {
    time: u64,
    order: u8,
    tie: u64,
    data: Message,
}

pub(super) struct Output {
    packets: Vec<Packet>,
    raw: bool,
    names: BTreeMap<u32, String>,
    tids: BTreeMap<u32, u64>,
    counters: std::collections::BTreeSet<u64>,
    slice_lanes: BTreeMap<(u32, u32), Vec<(u64, u64)>>,
    next_lane: u64,
}

impl Output {
    pub fn new(c: &Capture, threads: &BTreeMap<u64, u32>, raw: bool) -> io::Result<Self> {
        let mut reserved = threads
            .keys()
            .filter_map(|id| c.koids.get(id).copied())
            .collect();
        let mut next = 0;
        let mut tids = BTreeMap::new();
        for (&id, &local) in threads {
            let tid = match c.koids.get(&id) {
                Some(&koid) => koid,
                None => u64::from(allocate_id(&mut next, &mut reserved)?),
            };
            tids.insert(local, tid);
        }
        let mut out = Self {
            packets: Vec::new(),
            raw,
            names: threads
                .iter()
                .map(|(&id, &tid)| {
                    (
                        tid,
                        c.names
                            .get(&id)
                            .cloned()
                            .unwrap_or_else(|| format!("thread {id:#x}")),
                    )
                })
                .collect(),
            tids,
            counters: Default::default(),
            slice_lanes: Default::default(),
            next_lane: 1 << 62,
        };
        let mut root = descriptor(1, "HypeR", None);
        let mut identity = metadata(c);
        identity["exported_threads"] = serde_json::json!(
            threads
                .iter()
                .map(|(id, tid)| (format!("{id:#x}"), out.tids[tid]))
                .collect::<BTreeMap<_, _>>()
        );
        root.text(14, &identity.to_string());
        out.push(0, 0, 60, root)?;
        out.process_tree(c, threads)?;
        scheduler::write(c, threads, &mut out)?;
        Ok(out)
    }

    fn push(&mut self, time: u64, order: u8, field: u32, body: Message) -> io::Result<()> {
        if self.packets.len() == self.packets.capacity() {
            self.packets.try_reserve(1024).map_err(io::Error::other)?;
        }
        let mut data = Message::default();
        data.number(8, time);
        data.number(10, 1); // trusted_packet_sequence_id: one independent exporter.
        data.message(field, body);
        self.packets.push(Packet {
            time,
            order,
            tie: 0,
            data,
        });
        Ok(())
    }

    fn process_tree(&mut self, c: &Capture, threads: &BTreeMap<u64, u32>) -> io::Result<()> {
        // Process descriptors still require int32 PIDs. Perfetto also creates
        // an implicit main thread per process, so exclude every actual TID.
        let mut reserved = self.tids.values().copied().collect();
        let mut next_pid = 0;
        let mut allocate_pid = || allocate_id(&mut next_pid, &mut reserved);
        let kernel = allocate_pid()?;
        let unknown = allocate_pid()?;
        let mut processes = BTreeMap::new();
        let mut tree = Message::default();
        for (koid, name) in &c.processes {
            let pid = allocate_pid()?;
            processes.insert(*koid, pid);
            let label = format!("{name}[{koid:#018x}]");
            add_process(&mut tree, pid, &label);
        }
        let mut machines = BTreeMap::new();
        for vm in c.vms.values() {
            if !machines.contains_key(&vm.name) {
                let pid = allocate_pid()?;
                machines.insert(vm.name.clone(), pid);
                add_process(&mut tree, pid, &format!("vm-{}", vm.name));
            }
        }
        add_process(&mut tree, kernel, "kernel");
        add_process(&mut tree, unknown, "threads");
        let idle: std::collections::BTreeSet<_> = super::timeline::runs(c)
            .into_iter()
            .filter(|r| super::scheduling::next_class(c, r.begin) == Some(3))
            .map(|r| r.thread)
            .collect();
        for (&source_id, &tid) in threads {
            let object_tid = self.tids[&tid];
            let task = c.tasks.get(&source_id);
            let pid = if let Some(vm) = c.vms.get(&source_id) {
                machines.get(&vm.name).copied().unwrap_or(unknown)
            } else if c.vcpu_threads.contains(&source_id) {
                unknown
            } else {
                match task {
                    Some(task) => task
                        .process
                        .and_then(|id| processes.get(&id).copied())
                        .unwrap_or(if task.process.is_none() {
                            kernel
                        } else {
                            unknown
                        }),
                    None => unknown,
                }
            };
            let name = c
                .names
                .get(&source_id)
                .cloned()
                .unwrap_or_else(|| format!("thread {source_id:#x}"));
            let mut thread = Message::default();
            thread.number(1, object_tid);
            thread.number(2, u64::from(pid));
            thread.text(3, &name);
            thread.number(
                5,
                u64::from(idle.contains(&source_id) || task.is_some_and(|t| t.idle)),
            );
            tree.message(2, thread);
            let mut desc = descriptor(track(2, tid), &name, None);
            let mut identity = Message::default();
            identity.number(1, u64::from(pid));
            identity.number(2, object_tid);
            identity.text(5, &name);
            desc.message(4, identity);
            desc.text(
                14,
                &match c.koids.get(&source_id) {
                    Some(object) => {
                        format!("Scheduler ThreadId {source_id:#x}; Thread KOID {object:#x}")
                    }
                    None => format!("Scheduler ThreadId {source_id:#x}; Thread KOID unavailable; export-local TID {object_tid}"),
                },
            );
            self.push(0, 0, 60, desc)?;
        }
        self.push(0, 0, 122, tree)
    }

    /// Shared activity exporter supplies events directly, without encoding and
    /// parsing a JSON document or retaining a second Value-based event graph.
    pub fn event(&mut self, value: Value) -> io::Result<()> {
        let pid = value["pid"].as_u64().unwrap_or(0) as u32;
        let tid = value["tid"].as_u64().unwrap_or(0) as u32;
        let name = value["name"].as_str().unwrap_or("");
        match value["ph"].as_str() {
            Some("M") => {
                if pid == 2 || pid == 5 || pid == 1 && !self.raw {
                    return Ok(());
                }
                let name = if pid == 1 && tid == 0 {
                    "HypeR kernel events"
                } else {
                    value["args"]["name"].as_str().unwrap_or("")
                };
                let parent = if tid == 0 { 1 } else { track(pid, 0) };
                self.push(0, 0, 60, descriptor(track(pid, tid), name, Some(parent)))
            }
            Some("X") if pid == 1 || pid == 5 => Ok(()), // GenericKernel owns execution.
            Some("C") => self.counter(pid, tid, &value),
            Some("X" | "i") => {
                let time = nanoseconds(&value["ts"]);
                let duration = nanoseconds(&value["dur"]);
                let slice = value["ph"] == "X" && duration != 0;
                let uuid = if matches!(pid, 2 | 4 | 7) && slice {
                    self.slice_lane(pid, tid, time, time.saturating_add(duration))?
                } else {
                    track(pid, tid)
                };
                let mut event = Message::default();
                event.number(9, if slice { 1 } else { 3 });
                event.number(11, uuid);
                event.text(23, name);
                if let Some(category) = value["cat"].as_str() {
                    event.text(22, category);
                }
                annotations(&mut event, &value["args"]);
                if let Some(id) = value["bind_id"]
                    .as_str()
                    .and_then(|s| s.strip_prefix("0x"))
                    .and_then(|s| u64::from_str_radix(s, 16).ok())
                {
                    event.fixed(if value["flow_out"] == true { 47 } else { 48 }, id);
                }
                self.push(time, 3, 11, event)?;
                if slice {
                    if let Some(packet) = self.packets.last_mut() {
                        packet.tie = u64::MAX - duration;
                    }
                    let mut end = Message::default();
                    end.number(9, 2);
                    end.number(11, uuid);
                    self.push(time.saturating_add(duration), 1, 11, end)?;
                    if let Some(packet) = self.packets.last_mut() {
                        packet.tie = duration;
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn slice_lane(&mut self, pid: u32, tid: u32, start: u64, end: u64) -> io::Result<u64> {
        // TrackEvent ends pop a stack, without a request ID. Give overlapping
        // intervals separate lanes, including RPCs that complete out of order.
        // Intervals arrive at completion, so a last-end watermark conservatively
        // handles enclosing intervals discovered after their children as well.
        let lanes = self.slice_lanes.entry((pid, tid)).or_default();
        let lane = lanes
            .iter()
            .position(|(last, _)| *last <= start)
            .unwrap_or(lanes.len());
        let fresh = lane == lanes.len();
        let uuid = if fresh {
            // Separate from base track IDs and bit-63 counter IDs. There can
            // be at most one new lane per bounded captured interval.
            let uuid = self.next_lane;
            self.next_lane += 1;
            lanes.push((end, uuid));
            uuid
        } else {
            lanes[lane].0 = end;
            lanes[lane].1
        };
        if fresh {
            let kind = if pid == 2 { "Activity" } else { "Requests" };
            self.push(
                0,
                0,
                60,
                descriptor(uuid, &format!("{kind} {lane}"), Some(track(pid, tid))),
            )?;
        }
        Ok(uuid)
    }

    fn counter(&mut self, pid: u32, tid: u32, value: &Value) -> io::Result<()> {
        let uuid = track(pid, tid) | (1 << 63);
        let name = value["name"].as_str().unwrap_or("Outstanding requests");
        // A counter is a distinct track, never a slice on the device lane.
        if self.counters.insert(uuid) {
            let mut desc = descriptor(uuid, name, Some(track(pid, 0)));
            desc.message(8, Message::default());
            self.push(0, 0, 60, desc)?;
        }
        let mut event = Message::default();
        event.number(9, 4);
        event.number(11, uuid);
        event.number(30, value["args"]["observed requests"].as_u64().unwrap_or(0));
        self.push(nanoseconds(&value["ts"]), 3, 11, event)
    }

    pub fn finish(mut self, output: &mut impl Write) -> io::Result<()> {
        // Endpoints are discovered at completion. Sort before serializing so
        // nested RPCs and migrated threads obey TrackEvent's ordering contract.
        self.packets.sort_by_key(|p| (p.time, p.order, p.tie));
        for (i, mut packet) in self.packets.into_iter().enumerate() {
            if i == 0 {
                packet.data.number(13, 1);
            } // SEQ_INCREMENTAL_STATE_CLEARED.
            let mut wrapper = Message::default();
            wrapper.message(1, packet.data);
            output.write_all(&wrapper.0)?;
        }
        Ok(())
    }
}

fn allocate_id(next: &mut u32, reserved: &mut std::collections::BTreeSet<u64>) -> io::Result<u32> {
    loop {
        *next = next
            .checked_add(1)
            .filter(|id| *id <= i32::MAX as u32)
            .ok_or_else(|| io::Error::other("too many trace identities"))?;
        if reserved.insert(u64::from(*next)) {
            return Ok(*next);
        }
    }
}

fn add_process(tree: &mut Message, pid: u32, name: &str) {
    let mut process = Message::default();
    process.number(1, u64::from(pid));
    process.text(3, name);
    tree.message(1, process);
}
fn track(pid: u32, tid: u32) -> u64 {
    (u64::from(pid) << 32) | u64::from(tid)
}
fn descriptor(uuid: u64, name: &str, parent: Option<u64>) -> Message {
    let mut d = Message::default();
    d.number(1, uuid);
    d.text(2, name);
    if let Some(parent) = parent {
        d.number(5, parent);
    }
    d
}
fn nanoseconds(us: &Value) -> u64 {
    us.as_f64().unwrap_or(0.0).mul_add(1000.0, 0.0).round() as u64
}
fn annotations(event: &mut Message, args: &Value) {
    if let Some(args) = args.as_object() {
        for (name, value) in args {
            if let Some(mut arg) = annotation(value) {
                arg.text(10, name);
                event.message(4, arg);
            }
        }
    }
}
fn annotation(value: &Value) -> Option<Message> {
    let mut arg = Message::default();
    match value {
        Value::Null => return None,
        Value::Bool(v) => arg.number(2, u64::from(*v)),
        Value::Number(v) => {
            if let Some(v) = v.as_u64() {
                arg.number(3, v);
            } else if let Some(v) = v.as_i64() {
                arg.number(4, v as u64);
            } else if let Some(v) = v.as_f64() {
                arg.fixed(5, v.to_bits());
            }
        }
        Value::String(v) => arg.text(6, v),
        Value::Object(entries) => {
            for (name, value) in entries {
                if let Some(mut entry) = annotation(value) {
                    entry.text(10, name);
                    arg.message(11, entry);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                if let Some(entry) = annotation(value) {
                    arg.message(12, entry);
                }
            }
        }
    }
    Some(arg)
}
