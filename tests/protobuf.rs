// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::{
    export,
    model::Task,
    tests::{add, capture},
};

/// Fixture deliberately includes a migrated thread, a blocking/waking interval,
/// same-time transitions, a 64-bit scheduler ID, and overlapping device requests.
fn native_capture() -> crate::model::Capture {
    let mut c = capture();
    let worker = (1u64 << 60) + 42;
    c.names.insert(worker, "storage-worker".into());
    c.names.insert(10, "client".into());
    c.tasks.insert(
        worker,
        Task {
            process: Some(100),
            idle: false,
        },
    );
    c.tasks.insert(
        10,
        Task {
            process: Some(100),
            idle: false,
        },
    );
    c.processes.insert(100, "fs-backend".into());
    // Same slot, different generations must stay distinct in Perfetto.
    c.koids.insert(worker, (1 << 32) + 200);
    c.koids.insert(10, (2 << 32) + 200);
    c.names.insert(1, "vcpu/0".into());
    c.vms.insert(
        1,
        crate::model::VmThread {
            name: "io".into(),
            vcpu: 0,
        },
    );
    c.vcpu_threads.insert(1);
    // A vCPU can have a kernel scheduler object while belonging to a named VM.
    c.tasks.insert(
        1,
        Task {
            process: None,
            idle: false,
        },
    );
    for (cpu, time, seq, prev, next, state) in [
        (0, 2000, 0, 1, worker, 1),
        (0, 3000, 1, worker, 10, 4),
        (0, 4000, 3, 10, worker, 1),
        (0, 5000, 4, worker, 10, 5),
        (1, 6000, 0, 1, worker, 1),
        (1, 7000, 1, worker, 1, 6),
    ] {
        add(
            &mut c,
            cpu,
            time,
            seq,
            "sched_switch",
            &[
                ("prev", prev),
                ("next", next),
                ("state", state),
                ("cause", 3 | (1 << 8)),
            ],
        );
    }
    add(
        &mut c,
        0,
        3500,
        2,
        "sched_wakeup",
        &[
            ("thread", worker),
            ("waker", 10),
            ("target_cpu", 0),
            ("wait", 5),
        ],
    );
    for (time, seq, name, tag) in [
        (8000, 2, "block_submit", 1),
        (8001, 3, "block_command", 1),
        (9000, 4, "block_submit", 2),
        (9001, 5, "block_command", 2),
        (10000, 6, "block_complete", 1),
        (11000, 7, "block_complete", 2),
    ] {
        let fields = if name == "block_command" {
            [
                ("domain", 90),
                ("tag", tag),
                ("slot", tag - 1),
                ("opcode", 0x88),
            ]
        } else {
            [("domain", 90), ("tag", tag), ("bytes", 4096), ("read", 1)]
        };
        add(&mut c, 1, time, seq, name, &fields);
    }
    add(&mut c, 0, 12000, 5, "sched_reap", &[("thread", worker)]);
    c.sort();
    c
}

#[test]
fn native_scheduler_and_io_import_fixture() -> Result<(), Box<dyn std::error::Error>> {
    let c = native_capture();
    let mut output = Vec::new();
    export::perfetto(&c, &mut output, false)?;
    assert!(output.windows(6).any(|part| part == b"Wakeup"));
    if let Some(path) = std::env::var_os("HYPER_TRACE_PROTO_SCHED_FIXTURE") {
        std::fs::write(path, &output)?;
    }
    // Exercise output failure propagation instead of silently truncating traces.
    struct Failed;
    impl std::io::Write for Failed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("output failed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert!(export::perfetto(&c, Failed, false).is_err());
    Ok(())
}

#[test]
fn native_wakeup_latency_does_not_bridge_missing_dispatches()
-> Result<(), Box<dyn std::error::Error>> {
    for gap in [true, false] {
        let mut c = native_capture();
        if gap {
            add(&mut c, 0, 3550, 9, "sched_request", &[("reason", 1)]);
        } else {
            add(
                &mut c,
                1,
                3750,
                2,
                "sched_switch",
                &[("prev", 1), ("next", (1u64 << 60) + 42), ("state", 1)],
            );
        }
        c.sort();
        let mut output = Vec::new();
        export::perfetto(&c, &mut output, false)?;
        assert!(!output.windows(6).any(|part| part == b"Wakeup"));
    }
    Ok(())
}

// Decode the emitted wire records independently of the exporter. In particular,
// an END record has only a track ID: it cannot select an overlapping request.
fn varint(bytes: &mut &[u8]) -> Result<u64, &'static str> {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first().ok_or("truncated varint")?;
        *bytes = rest;
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(value);
        }
    }
    Err("oversized varint")
}
fn fields(mut bytes: &[u8]) -> Result<Vec<(u64, &[u8])>, &'static str> {
    let mut result = Vec::new();
    while !bytes.is_empty() {
        let key = varint(&mut bytes)?;
        let size = match key & 7 {
            0 => {
                let mut rest = bytes;
                varint(&mut rest)?;
                bytes.len() - rest.len()
            }
            1 => 8,
            2 => usize::try_from(varint(&mut bytes)?).map_err(|_| "field too large")?,
            5 => 4,
            _ => return Err("unsupported wire type"),
        };
        let value = bytes.get(..size).ok_or("truncated field")?;
        bytes = &bytes[size..];
        result.push((key >> 3, value));
    }
    Ok(result)
}
fn field(bytes: &[u8], key: u64) -> Result<&[u8], &'static str> {
    fields(bytes)?
        .into_iter()
        .find_map(|(number, value)| (number == key).then_some(value))
        .ok_or("missing field")
}
fn number(bytes: &[u8], key: u64) -> Result<u64, &'static str> {
    varint(&mut field(bytes, key)?)
}

#[test]
fn native_thread_ids_use_collected_koids() -> Result<(), Box<dyn std::error::Error>> {
    use hyper_os::inspect::trace::{Category, TraceRecord};
    let mut c = capture();
    let identities = [
        (10, Some((1 << 32) + 200)),
        (20, Some((2 << 32) + 200)),
        (30, Some(u64::MAX - 7)),
        (40, Some(1)), // Even a small test KOID must not collide with fallbacks/PIDs.
        (50, None),    // Old traces or lost metadata have no observed KOID.
    ];
    for (sequence, (thread, koid)) in identities.into_iter().enumerate() {
        assert!(c.add(
            0,
            TraceRecord {
                sequence: sequence as u64,
                timestamp_ns: 2000 + sequence as u64,
                category: Category::Scheduler,
                name: "sched_reap",
                source: "test",
                line: 1,
                fields: [
                    ("thread", thread),
                    ("koid", koid.unwrap_or(0)),
                    ("", 0),
                    ("", 0)
                ],
                field_count: if koid.is_some() { 2 } else { 1 },
            }
        )?);
    }
    assert_eq!(
        c.koids,
        identities
            .into_iter()
            .filter_map(|(id, koid)| koid.map(|k| (id, k)))
            .collect()
    );
    let mut output = Vec::new();
    export::perfetto(&c, &mut output, false)?;
    let mut trees = std::collections::BTreeSet::new();
    let mut descriptors = std::collections::BTreeSet::new();
    let mut states = std::collections::BTreeSet::new();
    let mut processes = std::collections::BTreeSet::new();
    for (_, packet) in fields(&output)? {
        if let Ok(tree) = field(packet, 122) {
            for (key, entry) in fields(tree)? {
                if key == 2 {
                    trees.insert(number(entry, 1)?);
                }
                if key == 1 {
                    processes.insert(number(entry, 1)?);
                }
            }
        }
        if let Ok(identity) = field(packet, 60).and_then(|d| field(d, 4)) {
            descriptors.insert(number(identity, 2)?);
        }
        if let Ok(state) = field(packet, 117) {
            states.insert(number(state, 3)?);
        }
    }
    let expected = [1, 2, (1 << 32) + 200, (2 << 32) + 200, u64::MAX - 7]
        .into_iter()
        .collect();
    assert_eq!(trees, expected);
    assert_eq!(descriptors, trees);
    assert_eq!(states, trees);
    assert!(processes.is_disjoint(&trees));
    assert!(
        output
            .windows(23)
            .any(|part| part == b"Thread KOID unavailable")
    );
    if let Some(path) = std::env::var_os("HYPER_TRACE_PROTO_KOID_FIXTURE") {
        std::fs::write(path, &output)?;
    }
    Ok(())
}

#[test]
fn native_retirement_preserves_observed_lifetime() -> Result<(), Box<dyn std::error::Error>> {
    for (case, thread, exit, reap) in [
        ("complete", 10, true, Some(4000)),
        ("same_time", 10, true, Some(3000)),
        ("missing_reap", 10, true, None),
        ("missing_exit", 10, false, Some(4000)),
        ("bootstrap", 0, false, Some(4000)),
        ("wide_id", (1u64 << 60) + 42, true, Some(4000)),
    ] {
        let mut c = capture();
        if exit {
            for (time, sequence, prev, next, state) in
                [(2000, 0, 1, thread, 1), (3000, 1, thread, 1, 6)]
            {
                add(
                    &mut c,
                    0,
                    time,
                    sequence,
                    "sched_switch",
                    &[("prev", prev), ("next", next), ("state", state)],
                );
            }
        }
        if let Some(time) = reap {
            // The reaper can run on a different CPU from the exiting thread.
            add(&mut c, 1, time, 0, "sched_reap", &[("thread", thread)]);
        }
        c.sort();
        let mut output = Vec::new();
        export::perfetto(&c, &mut output, false)?;
        let mut actual = Vec::new();
        let mut tids = std::collections::BTreeSet::new();
        for (_, packet) in fields(&output)? {
            if let Ok(state) = field(packet, 117) {
                actual.push((number(packet, 8)? + c.info.started_ns, number(state, 4)?));
                tids.insert(number(state, 3)?);
            }
        }
        let mut expected = Vec::new();
        if exit {
            expected.push((2000, 3));
            if reap != Some(3000) {
                expected.push((3000, 7));
            }
        }
        if let Some(time) = reap {
            expected.push((time, 8));
        }
        assert_eq!(actual, expected, "{case}");
        assert_eq!(tids.len(), 1, "{case}");
        if let Some(directory) = std::env::var_os("HYPER_TRACE_PROTO_RETIREMENT_FIXTURES") {
            std::fs::write(
                std::path::Path::new(&directory).join(format!("{case}.pftrace")),
                &output,
            )?;
        }
    }
    Ok(())
}

#[test]
fn interleaved_rpc_intervals_preserve_wire_endpoints() -> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    // Crossing, enclosing, identical, adjacent and zero-duration intervals.
    // Request 5 completes after its nested requests and is discovered last.
    let intervals = [
        (1, 2000, 5000),
        (2, 3000, 6000),
        (3, 6000, 7000),
        (4, 3000, 6000),
        (5, 1500, 8000),
        (6, 9000, 9000),
    ];
    let mut events = Vec::new();
    for (request, start, end) in intervals {
        events.push((start, "rpc_client_begin", request));
        events.push((end, "rpc_client_end", request));
    }
    events.sort_by_key(|e| e.0);
    for (sequence, (time, name, request)) in events.into_iter().enumerate() {
        add(
            &mut c,
            0,
            time,
            sequence as u64,
            name,
            &[("connection", 2), ("request", request), ("thread", 10)],
        );
    }
    c.sort();
    let mut output = Vec::new();
    export::perfetto(&c, &mut output, false)?;
    let mut pending = std::collections::BTreeMap::new();
    let mut actual = std::collections::BTreeMap::new();
    let mut lanes = std::collections::BTreeSet::new();
    for (_, packet) in fields(&output)? {
        let Ok(event) = field(packet, 11) else {
            continue;
        };
        let time = number(packet, 8)? + c.info.started_ns;
        let track = number(event, 11)?;
        match number(event, 9)? {
            1 | 3 => {
                assert_eq!(field(event, 23)?, b"RPC round trip");
                let annotation = fields(event)?
                    .into_iter()
                    .filter(|(key, _)| *key == 4)
                    .find_map(|(_, arg)| (field(arg, 10).ok() == Some(b"request")).then_some(arg))
                    .ok_or("missing request ID")?;
                let request = std::str::from_utf8(field(annotation, 6)?)?;
                let request = u64::from_str_radix(request.trim_start_matches("0x"), 16)?;
                if number(event, 9)? == 1 {
                    assert!(pending.insert(track, (request, time)).is_none());
                    lanes.insert(track);
                } else {
                    actual.insert(request, (time, time));
                }
            }
            2 => {
                let (request, start) = pending.remove(&track).ok_or("unmatched END")?;
                actual.insert(request, (start, time));
            }
            _ => return Err("unexpected event".into()),
        }
    }
    assert!(pending.is_empty());
    assert_eq!(
        actual,
        intervals
            .into_iter()
            .map(|(id, a, b)| (id, (a, b)))
            .collect()
    );
    // Five nonempty intervals need only four lanes: the adjacent call reuses
    // a completed lane; the enclosing call gets its own one.
    assert_eq!(lanes.len(), 4);
    if let Some(path) = std::env::var_os("HYPER_TRACE_PROTO_INTERLEAVED_RPC_FIXTURE") {
        std::fs::write(path, &output)?;
    }
    Ok(())
}

#[test]
fn retired_thread_keeps_process_group_from_recorded_syscalls()
-> Result<(), Box<dyn std::error::Error>> {
    use hyper_os::inspect::trace::{Category, TraceRecord};
    let thread = 0x8e000010;
    let thread_koid = (9u64 << 32) | 33;
    let process = (4u64 << 32) | 69;
    // Collection drains CPUs independently: retirement can arrive before entry.
    for reap_first in [false, true] {
        for named in [false, true] {
            let mut c = capture();
            if named {
                c.processes.insert(process, "ls".into());
                c.tasks.insert(
                    thread,
                    Task {
                        process: Some(process),
                        idle: false,
                    },
                );
                c.names.insert(thread, "ls-main".into());
            }
            let syscall = TraceRecord {
                sequence: 0,
                timestamp_ns: 2000,
                category: Category::Syscall,
                name: "syscall_enter",
                source: "test",
                line: 1,
                fields: [
                    ("thread", thread),
                    ("process", process),
                    ("kind", 13),
                    ("operation", 256),
                ],
                field_count: 4,
            };
            let reap = TraceRecord {
                sequence: 0,
                timestamp_ns: 3000,
                category: Category::Scheduler,
                name: "sched_reap",
                source: "test",
                line: 2,
                fields: [("thread", thread), ("koid", thread_koid), ("", 0), ("", 0)],
                field_count: 2,
            };
            if reap_first {
                c.add(1, reap)?;
                c.add(0, syscall)?;
            } else {
                c.add(0, syscall)?;
                c.add(1, reap)?;
            }
            assert_eq!(c.tasks.get(&thread).and_then(|t| t.process), Some(process));
            assert_eq!(c.koids.get(&thread), Some(&thread_koid));
            let expected_name = if named { "ls" } else { "process" };
            assert_eq!(
                c.processes.get(&process).map(String::as_str),
                Some(expected_name)
            );
            c.sort();
            let mut output = Vec::new();
            export::perfetto(&c, &mut output, false)?;
            let mut owner_pid = None;
            let mut thread_pid = None;
            for (_, packet) in fields(&output)? {
                if let Ok(tree) = field(packet, 122) {
                    for (key, entry) in fields(tree)? {
                        if key == 1
                            && field(entry, 3)?
                                == format!("{expected_name}[{process:#018x}]").as_bytes()
                        {
                            owner_pid = Some(number(entry, 1)?);
                        }
                        if key == 2 && number(entry, 1)? == thread_koid {
                            thread_pid = Some(number(entry, 2)?);
                        }
                    }
                }
            }
            assert!(owner_pid.is_some());
            assert_eq!(thread_pid, owner_pid);
        }
    }
    Ok(())
}

#[test]
fn absent_process_observation_does_not_invent_a_parent() -> Result<(), Box<dyn std::error::Error>> {
    use hyper_os::inspect::trace::{Category, TraceRecord};
    for (name, count) in [("syscall_enter", 1), ("syscall_enter", 2), ("unrelated", 2)] {
        let mut c = capture();
        c.add(
            0,
            TraceRecord {
                sequence: 0,
                timestamp_ns: 2000,
                category: Category::Syscall,
                name,
                source: "test",
                line: 1,
                fields: [
                    ("thread", 42),
                    ("process", if name == "unrelated" { 100 } else { 0 }),
                    ("", 0),
                    ("", 0),
                ],
                field_count: count,
            },
        )?;
        assert!(c.tasks.is_empty());
        assert!(c.processes.is_empty());
    }
    Ok(())
}
