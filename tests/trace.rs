// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0
use crate::{export, model, options};
use hyper_os::inspect::trace::TraceInfo;
use model::{Capture, Event, Site};
use serde_json::Value;
pub(super) fn capture() -> Capture {
    Capture::new(
        TraceInfo {
            generation: 1,
            started_ns: 1000,
            deadline_ns: 1_000_000,
            categories: 127,
            cpu_count: 2,
            capacity: 64,
            maximum_capacity: 16384,
            running: 0,
            cpu_mask: core::array::from_fn(|i| if i == 0 { 3 } else { 0 }),
            revision: [0; 64],
        },
        128,
    )
}
pub(super) fn add(
    c: &mut Capture,
    cpu: usize,
    time: u64,
    sequence: u64,
    name: &str,
    fields: &[(&str, u64)],
) {
    let site = c.sites.len();
    c.sites.push(Site {
        category: "test".into(),
        name: name.into(),
        source: "test\"file".into(),
        line: 1,
        fields: fields.iter().map(|(n, _)| n.to_string()).collect(),
    });
    let mut values = [0; 4];
    for (i, (_, v)) in fields.iter().enumerate() {
        values[i] = *v;
    }
    c.events.push(Event {
        time,
        sequence,
        cpu,
        site,
        values,
    });
}
fn json(c: &mut Capture) -> Result<Value, Box<dyn std::error::Error>> {
    render(c, true)
}
fn render(c: &mut Capture, raw_events: bool) -> Result<Value, Box<dyn std::error::Error>> {
    c.sort();
    let mut bytes = Vec::new();
    export::chrome(c, &mut bytes, raw_events)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[test]
fn thread_lanes_follow_migration_without_raw_points() -> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    c.names.insert(42, "io-worker".into());
    for (cpu, start, end) in [(0, 2000, 4000), (1, 5000, 7000)] {
        add(
            &mut c,
            cpu,
            start,
            0,
            "sched_switch",
            &[("prev", 1), ("next", 42)],
        );
        add(
            &mut c,
            cpu,
            end,
            1,
            "sched_switch",
            &[("prev", 42), ("next", 1)],
        );
    }
    let value = render(&mut c, false)?;
    let runs: Vec<_> = spans(&value)
        .into_iter()
        .filter(|e| e["pid"] == 5)
        .collect();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["tid"], runs[1]["tid"]);
    for (cpu, run) in runs.into_iter().enumerate() {
        assert_eq!(run["name"], "Running");
        assert_eq!(run["args"]["cpu"], cpu);
        assert_eq!(run["args"]["switch_in_sequence"], 0);
        assert_eq!(run["args"]["switch_out_sequence"], 1);
    }
    assert!(
        !value["traceEvents"]
            .as_array()
            .ok_or("missing events")?
            .iter()
            .any(|e| e["name"] == "sched_switch")
    );
    Ok(())
}

#[test]
fn explicit_bootstrap_switch_preserves_zero_scheduler_id() -> Result<(), Box<dyn std::error::Error>>
{
    let mut c = capture();
    c.names.insert(0, "bootstrap".into());
    add(
        &mut c,
        0,
        2000,
        0,
        "sched_switch",
        &[("prev", 1), ("next", 0), ("state", 1)],
    );
    add(
        &mut c,
        0,
        3000,
        1,
        "sched_switch",
        &[("prev", 0), ("next", 1), ("state", 1)],
    );
    let value = render(&mut c, false)?;
    assert!(
        spans(&value)
            .iter()
            .any(|e| e["pid"] == 5 && e["args"]["thread"] == "0x0")
    );
    Ok(())
}

#[test]
fn overlapping_cpu_observations_do_not_fabricate_parallel_thread_execution()
-> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    for (cpu, start, end) in [(0, 2000, 6000), (1, 4000, 8000)] {
        c.losses[cpu].incomplete = true;
        add(
            &mut c,
            cpu,
            start,
            0,
            "sched_switch",
            &[("prev", 1), ("next", 42)],
        );
        add(
            &mut c,
            cpu,
            end,
            1,
            "sched_switch",
            &[("prev", 42), ("next", 1)],
        );
    }
    let value = render(&mut c, false)?;
    assert!(spans(&value).is_empty());
    assert_eq!(value["metadata"]["cross_cpu_spans"], false);
    Ok(())
}

#[test]
fn scheduling_windows_reject_missing_or_inconsistent_boundaries()
-> Result<(), Box<dyn std::error::Error>> {
    for mode in ["gap", "identity", "missing-next"] {
        let mut c = capture();
        c.losses[0].incomplete = true;
        add(
            &mut c,
            0,
            2000,
            0,
            "sched_switch",
            &[("prev", 1), ("next", 2)],
        );
        if mode == "missing-next" {
            add(&mut c, 0, 3000, 1, "sched_switch", &[("prev", 2)]);
        }
        add(
            &mut c,
            0,
            4000,
            if mode == "identity" { 1 } else { 2 },
            "sched_switch",
            &[
                ("prev", if mode == "identity" { 7 } else { 2 }),
                ("next", 3),
            ],
        );
        let value = render(&mut c, false)?;
        assert!(spans(&value).is_empty(), "{mode}");
    }
    Ok(())
}

#[test]
fn other_cpu_gaps_do_not_erase_local_execution_windows() -> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    add(
        &mut c,
        0,
        2000,
        0,
        "sched_switch",
        &[("prev", 1), ("next", 2)],
    );
    add(&mut c, 1, 3000, 0, "irq_enter", &[("irq", 26)]);
    add(&mut c, 1, 4000, 2, "irq_exit", &[("irq", 26)]);
    add(
        &mut c,
        0,
        5000,
        1,
        "sched_switch",
        &[("prev", 2), ("next", 1)],
    );
    let value = render(&mut c, false)?;
    assert_eq!(spans(&value).len(), 2);
    assert!(spans(&value).iter().all(|e| e["args"]["cpu"] == 0));
    Ok(())
}

#[test]
fn perfetto_cli_defaults_to_timelines() -> Result<(), Box<dyn std::error::Error>> {
    use clap::Parser;
    #[derive(Parser)]
    struct Args {
        #[command(subcommand)]
        command: options::Command,
    }
    let args = Args::try_parse_from(["trace", "record"])?;
    let options::Command::Record { export, .. } = args.command else {
        return Err("wrong command".into());
    };
    assert!(!export.raw_events);
    assert!(matches!(export.format, options::Format::Perfetto));
    assert_eq!(export.output, "/tmp/trace.pftrace");
    Ok(())
}
fn spans(value: &Value) -> Vec<&Value> {
    value["traceEvents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["ph"] == "X")
        .collect()
}

fn flows(value: &Value) -> Vec<&Value> {
    value["traceEvents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["flow_out"] == true)
        .collect()
}

#[test]
fn causal_delivery_is_directional_and_needs_committed_receive()
-> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    // Same protocol ID in opposite directions, plus an undelivered send and
    // orphan receive. No thread adjacency can connect either orphan.
    add(
        &mut c,
        0,
        2000,
        0,
        "ipc_enqueue",
        &[("queue", 2), ("message", 7), ("thread", 10), ("bytes", 8)],
    );
    add(
        &mut c,
        1,
        3000,
        0,
        "ipc_enqueue",
        &[("queue", 3), ("message", 7), ("thread", 20), ("bytes", 8)],
    );
    add(
        &mut c,
        0,
        4000,
        1,
        "ipc_enqueue",
        &[("queue", 2), ("message", 8), ("thread", 10)],
    );
    add(
        &mut c,
        1,
        5000,
        1,
        "ipc_dequeue",
        &[("queue", 2), ("message", 7), ("thread", 20)],
    );
    add(
        &mut c,
        0,
        6000,
        2,
        "ipc_dequeue",
        &[("queue", 3), ("message", 7), ("thread", 10)],
    );
    add(
        &mut c,
        1,
        7000,
        2,
        "ipc_dequeue",
        &[("queue", 4), ("message", 8), ("thread", 20)],
    );
    let value = json(&mut c)?;
    assert_eq!(flows(&value).len(), 2);
    assert_ne!(flows(&value)[0]["bind_id"], flows(&value)[1]["bind_id"]);
    c.losses[1].overwritten = 1;
    assert_eq!(flows(&json(&mut c)?).len(), 2);
    c.losses[1].overwritten = 0;
    c.info.cpu_mask[0] = 1;
    assert_eq!(flows(&json(&mut c)?).len(), 2);
    Ok(())
}

#[test]
fn equal_time_message_delivery_does_not_depend_on_cpu_or_read_order()
-> Result<(), Box<dyn std::error::Error>> {
    for prefix in ["ipc", "cap"] {
        for send_cpu in 0..2 {
            for receive_first in [false, true] {
                let mut c = capture();
                let send = (send_cpu, format!("{prefix}_enqueue"), 10);
                let receive = (1 - send_cpu, format!("{prefix}_dequeue"), 20);
                let boundaries = if receive_first {
                    [receive, send]
                } else {
                    [send, receive]
                };
                for (cpu, name, thread) in boundaries {
                    add(
                        &mut c,
                        cpu,
                        2000,
                        0,
                        &name,
                        &[("queue", 2), ("message", 7), ("thread", thread)],
                    );
                }
                let value = json(&mut c)?;
                let outgoing = flows(&value);
                assert_eq!(outgoing.len(), 1, "{prefix}, sender CPU {send_cpu}");
                assert_eq!(outgoing[0]["args"]["thread"], "0xa");
                let incoming =
                    value["traceEvents"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|event| {
                            event["flow_in"] == true && event["bind_id"] == outgoing[0]["bind_id"]
                        });
                assert_eq!(
                    incoming.map(|event| &event["args"]["thread"]),
                    Some(&serde_json::json!("0x14"))
                );
                assert_eq!(
                    incoming.map(|event| &event["args"]["latency_ns"]),
                    Some(&serde_json::json!(0))
                );
                if prefix == "ipc"
                    && send_cpu == 1
                    && receive_first
                    && let Some(path) = std::env::var_os("HYPER_TRACE_EQUAL_TIME_FIXTURE")
                {
                    std::fs::write(path, serde_json::to_vec(&value)?)?;
                }
                c.losses[0].incomplete = true;
                assert_eq!(flows(&json(&mut c)?).len(), 1);
            }
        }
    }
    Ok(())
}

#[test]
fn earlier_or_mismatched_receives_do_not_form_delivery_flows()
-> Result<(), Box<dyn std::error::Error>> {
    for (send_time, receive_message) in [(3000, 7), (2000, 8)] {
        let mut c = capture();
        add(
            &mut c,
            0,
            2000,
            0,
            "ipc_dequeue",
            &[("queue", 2), ("message", receive_message), ("thread", 20)],
        );
        add(
            &mut c,
            1,
            send_time,
            0,
            "ipc_enqueue",
            &[("queue", 2), ("message", 7), ("thread", 10)],
        );
        assert!(flows(&json(&mut c)?).is_empty());
    }
    Ok(())
}

#[test]
fn loss_keeps_block_boundaries_raw_without_inventing_command_slots()
-> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    add(
        &mut c,
        0,
        2000,
        0,
        "block_complete",
        &[("domain", 1), ("tag", 42)],
    );
    add(
        &mut c,
        1,
        2000,
        0,
        "block_submit",
        &[("domain", 1), ("tag", 42), ("read", 1)],
    );
    c.losses[1].incomplete = true;
    let v = json(&mut c)?;
    assert!(spans(&v).iter().all(|e| e["pid"] != 4));
    for name in ["block_submit", "block_complete"] {
        assert!(
            v["traceEvents"]
                .as_array()
                .ok_or("missing events")?
                .iter()
                .any(|e| e["ph"] == "i" && e["name"] == name)
        );
    }
    Ok(())
}

#[test]
fn explicit_rpc_parent_wait_and_message_flows_import_fixture()
-> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    add(
        &mut c,
        0,
        2000,
        0,
        "rpc_client_begin",
        &[
            ("connection", 2),
            ("request", 1),
            ("thread", 10),
            ("operation", 3),
        ],
    );
    add(
        &mut c,
        0,
        3000,
        1,
        "ipc_enqueue",
        &[("queue", 3), ("message", 1), ("thread", 10), ("bytes", 16)],
    );
    add(
        &mut c,
        0,
        4000,
        2,
        "wait_block",
        &[("thread", 10), ("wait", 1)],
    );
    add(
        &mut c,
        1,
        5000,
        0,
        "ipc_dequeue",
        &[("queue", 3), ("message", 1), ("thread", 20)],
    );
    add(
        &mut c,
        1,
        6000,
        1,
        "rpc_server_begin",
        &[
            ("connection", 2),
            ("request", 1),
            ("thread", 20),
            ("operation", 3),
        ],
    );
    add(
        &mut c,
        1,
        7000,
        2,
        "rpc_client_begin",
        &[
            ("connection", 4),
            ("request", 9),
            ("thread", 20),
            ("operation", 5),
        ],
    );
    add(
        &mut c,
        1,
        8000,
        3,
        "rpc_parent",
        &[
            ("connection", 4),
            ("request", 9),
            ("parent_connection", 2),
            ("parent_request", 1),
        ],
    );
    add(
        &mut c,
        1,
        9000,
        4,
        "rpc_client_end",
        &[
            ("connection", 4),
            ("request", 9),
            ("thread", 20),
            ("status", 7),
        ],
    );
    add(
        &mut c,
        1,
        10000,
        5,
        "cap_enqueue",
        &[("queue", 2), ("message", 100), ("thread", 20)],
    );
    add(
        &mut c,
        1,
        11000,
        6,
        "wait_complete",
        &[("thread", 10), ("wait", 1), ("outcome", 1), ("waker", 20)],
    );
    add(
        &mut c,
        1,
        12000,
        7,
        "signal_wake",
        &[("thread", 10), ("wait", 1), ("source_id", 2)],
    );
    add(
        &mut c,
        1,
        13000,
        8,
        "sched_wakeup",
        &[("thread", 10), ("wait", 1), ("target_cpu", 0)],
    );
    add(
        &mut c,
        1,
        14000,
        9,
        "rpc_server_end",
        &[
            ("connection", 2),
            ("request", 1),
            ("thread", 20),
            ("status", 0),
        ],
    );
    add(
        &mut c,
        0,
        15000,
        3,
        "sched_switch",
        &[("prev", 30), ("next", 10), ("state", 4)],
    );
    add(
        &mut c,
        0,
        16000,
        4,
        "cap_dequeue",
        &[("queue", 2), ("message", 100), ("thread", 10)],
    );
    add(
        &mut c,
        0,
        17000,
        5,
        "rpc_client_end",
        &[
            ("connection", 2),
            ("request", 1),
            ("thread", 10),
            ("status", 0),
        ],
    );
    let value = json(&mut c)?;
    assert_eq!(flows(&value).len(), 6);
    assert!(spans(&value).iter().any(|e| e["name"] == "RPC handler"));
    assert!(
        spans(&value)
            .iter()
            .any(|e| e["name"] == "Blocked (notified)")
    );
    assert!(
        spans(&value)
            .iter()
            .any(|e| e["name"] == "RPC round trip" && e["args"]["completion"]["status"] == 7)
    );
    // Optional artifact for the real upstream TraceProcessor import contract.
    if let Some(path) = std::env::var_os("HYPER_TRACE_FIXTURE") {
        std::fs::write(path, serde_json::to_vec(&value)?)?;
    }
    if let Some(path) = std::env::var_os("HYPER_TRACE_PROTO_RPC_FIXTURE") {
        let mut output = Vec::new();
        export::perfetto(&c, &mut output, false)?;
        std::fs::write(path, output)?;
    }
    Ok(())
}

#[test]
fn missing_rpc_boundaries_and_duplicate_active_ids_are_not_joined()
-> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    add(
        &mut c,
        0,
        2000,
        0,
        "rpc_client_begin",
        &[("connection", 2), ("request", 1), ("thread", 10)],
    );
    add(
        &mut c,
        1,
        3000,
        0,
        "rpc_client_begin",
        &[("connection", 2), ("request", 1), ("thread", 20)],
    );
    add(
        &mut c,
        0,
        4000,
        1,
        "rpc_client_end",
        &[("connection", 2), ("request", 1), ("thread", 10)],
    );
    add(
        &mut c,
        1,
        5000,
        1,
        "rpc_server_begin",
        &[("connection", 4), ("request", 1), ("thread", 20)],
    );
    add(
        &mut c,
        0,
        6000,
        2,
        "rpc_client_end",
        &[("connection", 4), ("request", 1), ("thread", 10)],
    );
    let value = json(&mut c)?;
    assert!(flows(&value).is_empty());
    assert!(spans(&value).is_empty());
    Ok(())
}

#[test]
fn late_duplicate_rpc_ids_invalidate_preceding_dispatch_edges()
-> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    for (sequence, name, thread) in [
        (0, "rpc_client_begin", 10),
        (1, "rpc_server_begin", 20),
        (2, "rpc_client_begin", 30),
        (3, "rpc_server_end", 20),
        (4, "rpc_client_end", 30),
    ] {
        add(
            &mut c,
            0,
            2000 + sequence * 1000,
            sequence,
            name,
            &[("connection", 2), ("request", 1), ("thread", thread)],
        );
    }
    let value = json(&mut c)?;
    assert!(flows(&value).is_empty());
    assert!(spans(&value).is_empty());
    assert_eq!(
        value["traceEvents"]
            .as_array()
            .map(|events| events.iter().filter(|event| event["ph"] == "i").count()),
        Some(5)
    );
    Ok(())
}

#[test]
fn completed_rpc_ids_can_be_reused_without_losing_correlations()
-> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    for round in 0..2 {
        for (index, name, thread) in [
            (0, "rpc_client_begin", 10),
            (1, "rpc_server_begin", 20),
            (2, "rpc_server_end", 20),
            (3, "rpc_client_end", 10),
        ] {
            let sequence = round * 4 + index;
            add(
                &mut c,
                0,
                2000 + sequence * 1000,
                sequence,
                name,
                &[("connection", 2), ("request", 1), ("thread", thread)],
            );
        }
    }
    let value = json(&mut c)?;
    assert_eq!(flows(&value).len(), 2);
    for name in ["RPC round trip", "RPC handler"] {
        assert_eq!(
            spans(&value)
                .iter()
                .filter(|event| event["name"] == name)
                .count(),
            2
        );
    }
    Ok(())
}
#[test]
fn migration_pairs_use_time_not_cpu_read_order() -> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    let id = 0xfedcba9876543210;
    add(
        &mut c,
        0,
        4000,
        0,
        "syscall_exit",
        &[("thread", id), ("status", 0)],
    );
    add(
        &mut c,
        1,
        2000,
        0,
        "syscall_enter",
        &[("thread", id), ("operation", 256)],
    );
    let value = json(&mut c)?;
    let items = spans(&value);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["dur"], 2.0);
    assert_eq!(items[0]["args"]["thread"], "0xfedcba9876543210");
    assert_eq!(items[0]["args"]["source"], "test\"file");
    Ok(())
}
#[test]
fn missing_events_never_form_a_complete_syscall() -> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    add(&mut c, 0, 2000, 0, "syscall_enter", &[("thread", 5)]);
    add(&mut c, 0, 4000, 2, "syscall_exit", &[("thread", 5)]);
    let value = json(&mut c)?;
    assert!(spans(&value).is_empty());
    assert!(
        value["traceEvents"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v["name"] == "TRACE GAP"))
    );
    Ok(())
}
#[test]
fn incomplete_data_keeps_names_and_capture_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    c.names.insert(2, "io-runtime".into());
    c.losses[0].incomplete = true;
    add(
        &mut c,
        0,
        2000,
        0,
        "sched_switch",
        &[("prev", 1), ("next", 2), ("state", 1)],
    );
    add(
        &mut c,
        0,
        4000,
        1,
        "sched_switch",
        &[("prev", 2), ("next", 1), ("state", 4)],
    );
    let value = render(&mut c, false)?;
    let runs = spans(&value);
    assert_eq!(runs.len(), 2);
    assert!(
        runs.iter()
            .any(|run| run["pid"] == 5 && run["name"] == "Running")
    );
    assert_eq!(value["metadata"]["losses"][0]["incomplete"], true);
    assert!(
        runs.iter()
            .any(|run| run["pid"] == 1 && run["name"] == "io-runtime")
    );
    assert!(
        !value["traceEvents"]
            .as_array()
            .ok_or("missing events")?
            .iter()
            .any(|e| matches!(
                e["name"].as_str(),
                Some("Incomplete trace data" | "TRACE LOSS" | "TRACE GAP")
            ))
    );
    Ok(())
}
#[test]
fn runnable_latency_and_cpu_spans_require_both_boundaries() -> Result<(), Box<dyn std::error::Error>>
{
    let mut c = capture();
    c.names.insert(2, "worker\"name".into());
    add(
        &mut c,
        1,
        2000,
        0,
        "sched_wakeup",
        &[("thread", 2), ("target_cpu", 0)],
    );
    add(
        &mut c,
        0,
        3000,
        0,
        "sched_switch",
        &[("prev", 1), ("next", 2), ("state", 4)],
    );
    add(
        &mut c,
        0,
        6000,
        1,
        "sched_switch",
        &[("prev", 2), ("next", 1), ("state", 4)],
    );
    let value = json(&mut c)?;
    let items = spans(&value);
    assert_eq!(items.len(), 3);
    assert!(
        items
            .iter()
            .any(|v| v["name"] == "Runnable" && v["dur"] == 1.0)
    );
    assert!(
        items
            .iter()
            .any(|v| v["name"] == "worker\"name" && v["dur"] == 3.0)
    );
    Ok(())
}
#[test]
fn jsonl_preserves_events_and_reports_incomplete_capture() -> Result<(), Box<dyn std::error::Error>>
{
    let mut c = capture();
    c.limited = true;
    add(&mut c, 0, 2000, 0, "one", &[("bytes", 4096)]);
    let mut out = Vec::new();
    export::jsonl(&c, &mut out)?;
    let rows: Vec<Value> = std::str::from_utf8(&out)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["limited"], true);
    assert_eq!(rows[1]["args"]["bytes"], 4096);
    assert_eq!(rows[2]["loss"], true);
    Ok(())
}
#[test]
fn filters_are_strict_and_support_more_than_64_cpus() -> Result<(), String> {
    assert_eq!(options::categories("scheduler,io")?, 9);
    assert_eq!(options::categories("all")?, 127);
    for bad in ["", "all,io", "sched", "scheduler,"] {
        assert!(options::categories(bad).is_err());
    }
    let mask = options::cpus("0,63-65,255")?.0;
    assert_eq!(mask, [1 | (1 << 63), 3, 0, 1 << 63]);
    for bad in ["", "-1", "256", "3-2", "1,,2", "all,0"] {
        assert!(options::cpus(bad).is_err());
    }
    Ok(())
}
#[test]
fn record_cli_covers_limits_output_and_workload() -> Result<(), Box<dyn std::error::Error>> {
    use clap::Parser;
    #[derive(Parser)]
    struct Args {
        #[command(subcommand)]
        command: options::Command,
    }
    let args = Args::try_parse_from([
        "trace",
        "record",
        "--duration-ms",
        "250",
        "--categories",
        "irq,syscall",
        "--cpus",
        "0-1",
        "--buffer-records",
        "64",
        "--max-events",
        "100",
        "--poll-ms",
        "1",
        "--format",
        "jsonl",
        "--output",
        "/tmp/a",
        "--no-names",
        "--raw-events",
        "--fail-on-loss",
        "--snapshot",
        "--",
        "/bin/echo",
        "ok",
    ])?;
    match args.command {
        options::Command::Record {
            capture,
            export,
            poll_ms,
            snapshot,
            command,
        } => {
            assert_eq!(capture.config().duration_ms, 250);
            assert_eq!(capture.categories, 36);
            assert_eq!(capture.buffer_records, 64);
            assert_eq!(export.max_events, 100);
            assert!(export.no_names && export.fail_on_loss && export.raw_events);
            assert_eq!(poll_ms, 1);
            assert!(snapshot);
            assert_eq!(command, ["/bin/echo", "ok"]);
        }
        _ => return Err("wrong command".into()),
    };
    for flag in [
        "--duration-ms",
        "--max-events",
        "--poll-ms",
        "--buffer-records",
    ] {
        assert!(Args::try_parse_from(["trace", "record", flag, "0"]).is_err());
    }
    for arguments in [
        vec!["trace", "start"],
        vec!["trace", "start", "--buffer-records", "16384"],
    ] {
        match Args::try_parse_from(arguments)?.command {
            options::Command::Start(capture) => assert_eq!(capture.config().capacity, 16384),
            _ => return Err("wrong command".into()),
        }
    }
    assert!(Args::try_parse_from(["trace", "start", "--buffer-records", "16385"]).is_err());
    Ok(())
}

#[test]
fn collection_interns_distinct_sites_and_enforces_limit() -> Result<(), Box<dyn std::error::Error>>
{
    use hyper_os::inspect::trace::{Category, TraceRecord};
    let mut c = capture();
    c.limit = 2;
    let record = |name, sequence| TraceRecord {
        sequence,
        timestamp_ns: sequence + 1000,
        category: Category::Io,
        name,
        source: "truncated/shared/path",
        line: 12,
        fields: [("bytes", 4096), ("", 0), ("", 0), ("", 0)],
        field_count: 1,
    };
    assert!(c.add(0, record("submit", 0))?);
    assert!(c.add(0, record("complete", 1))?);
    assert_eq!(c.sites.len(), 2);
    assert!(!c.add(0, record("submit", 2))?);
    assert!(c.limited);
    assert_eq!(c.events.len(), 2);
    Ok(())
}

#[test]
fn failed_syscall_and_io_completion_preserve_outcomes() -> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    add(
        &mut c,
        0,
        2000,
        0,
        "syscall_enter",
        &[("thread", 5), ("operation", 256)],
    );
    add(
        &mut c,
        0,
        3000,
        1,
        "syscall_exit",
        &[("thread", 5), ("status", (-4i64) as u64)],
    );
    add(
        &mut c,
        1,
        2000,
        0,
        "block_submit",
        &[("domain", 7), ("tag", 1), ("bytes", 512), ("read", 1)],
    );
    add(
        &mut c,
        1,
        3000,
        1,
        "block_command",
        &[("domain", 7), ("tag", 1), ("slot", 0), ("opcode", 0x88)],
    );
    add(
        &mut c,
        1,
        4000,
        2,
        "block_complete",
        &[("domain", 7), ("tag", 1), ("success", 0)],
    );
    let value = json(&mut c)?;
    let spans = spans(&value);
    assert!(
        spans
            .iter()
            .any(|s| s["name"] == "syscall" && s["args"]["completion"]["status"] == -4)
    );
    assert!(
        spans
            .iter()
            .any(|s| s["name"] == "read" && s["args"]["completion"]["success"] == 0)
    );
    Ok(())
}

#[test]
fn cpu_filter_cannot_claim_cross_cpu_spans_but_keeps_local_irq_time()
-> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    c.info.cpu_mask[0] = 1;
    add(&mut c, 0, 2000, 0, "syscall_enter", &[("thread", 5)]);
    add(&mut c, 0, 3000, 1, "irq_enter", &[("irq", 26)]);
    add(&mut c, 0, 4000, 2, "irq_exit", &[("irq", 26)]);
    add(&mut c, 0, 5000, 3, "syscall_exit", &[("thread", 5)]);
    let value = json(&mut c)?;
    let spans = spans(&value);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0]["name"], "IRQ 26");
    assert_eq!(spans[0]["dur"], 1.0);
    Ok(())
}

#[test]
fn lost_other_cpu_prefix_or_tail_cannot_join_distinct_invocations()
-> Result<(), Box<dyn std::error::Error>> {
    for overwritten_prefix in [false, true] {
        let mut c = capture();
        if overwritten_prefix {
            c.losses[1].overwritten = 2;
        } else {
            c.limited = true;
        }
        // CPU 1's missing records may contain the first invocation's return
        // and a second invocation's entry. CPU 0 alone looks contiguous.
        add(&mut c, 0, 2000, 0, "syscall_enter", &[("thread", 5)]);
        add(&mut c, 0, 3000, 1, "irq_enter", &[("irq", 26)]);
        add(&mut c, 0, 4000, 2, "irq_exit", &[("irq", 26)]);
        add(&mut c, 0, 5000, 3, "syscall_exit", &[("thread", 5)]);
        let value = json(&mut c)?;
        let spans = spans(&value);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0]["name"], "IRQ 26");
        assert_eq!(value["metadata"]["cross_cpu_spans"], false);
    }
    Ok(())
}

#[test]
fn switch_causes_are_explicit_and_never_inferred_from_ready_state()
-> Result<(), Box<dyn std::error::Error>> {
    for (reason, point, name, preempted) in [
        (1, 3, "yield", Some(false)),
        (2, 3, "preempt", Some(true)),
        (3, 3, "block", Some(false)),
        (4, 3, "exit", Some(false)),
        (5, 3, "migrate", Some(true)),
        (5, 2, "migrate", Some(true)),
        (5, 1, "migrate", Some(false)),
        (5, 99, "migrate", None),
        (6, 3, "idle-dispatch", Some(false)),
        (99, 3, "unknown", None),
    ] {
        let mut c = capture();
        add(
            &mut c,
            0,
            2000,
            0,
            "sched_switch",
            &[("prev", 1), ("next", 2), ("state", 1)],
        );
        add(
            &mut c,
            0,
            3000,
            1,
            "sched_switch",
            &[
                ("prev", 2),
                ("next", 3),
                ("state", 1 | (2 << 8)),
                ("cause", reason | (point << 8)),
            ],
        );
        let value = json(&mut c)?;
        let slices: Vec<_> = spans(&value)
            .into_iter()
            .filter(|s| s["pid"] == 1)
            .collect();
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0]["args"]["switch_out_reason"], name);
        let switch = value["traceEvents"]
            .as_array()
            .ok_or("missing events")?
            .iter()
            .rev()
            .find(|e| e["name"] == "sched_switch")
            .ok_or("missing switch")?;
        assert_eq!(switch["args"]["prev_state"], 1);
        assert_eq!(switch["args"]["next_class"], 2);
        assert_eq!(slices[0]["args"]["preempted"], serde_json::json!(preempted));
        assert_eq!(
            slices[0]["args"]["schedule_point"],
            match point {
                1 => "explicit",
                2 => "safe-point",
                3 => "irq-tail",
                _ => "unknown",
            }
        );
    }
    // Missing cause metadata must not infer preemption from Ready state.
    let mut c = capture();
    add(
        &mut c,
        0,
        2000,
        0,
        "sched_switch",
        &[("prev", 1), ("next", 2), ("state", 1)],
    );
    let value = json(&mut c)?;
    let switch = value["traceEvents"]
        .as_array()
        .ok_or("missing events")?
        .iter()
        .find(|e| e["name"] == "sched_switch")
        .ok_or("missing switch")?;
    assert!(switch["args"]["preempted"].is_null());
    Ok(())
}

#[test]
fn switch_cause_is_atomic_even_after_loss() -> Result<(), Box<dyn std::error::Error>> {
    for incomplete in [false, true] {
        let mut c = capture();
        add(
            &mut c,
            0,
            2000,
            0,
            "sched_switch",
            &[
                ("prev", 1),
                ("next", 2),
                ("state", 1),
                ("cause", 1 | (1 << 8)),
            ],
        );
        add(
            &mut c,
            0,
            3000,
            2,
            "sched_switch",
            &[
                ("prev", 2),
                ("next", 3),
                ("state", 1),
                ("cause", 2 | (3 << 8)),
            ],
        );
        c.losses[0].incomplete = incomplete;
        let value = json(&mut c)?;
        assert!(spans(&value).is_empty());
        let switch = value["traceEvents"]
            .as_array()
            .ok_or("missing events")?
            .iter()
            .rev()
            .find(|e| e["name"] == "sched_switch")
            .ok_or("missing switch")?;
        assert_eq!(switch["args"]["preempted"], true);
        assert_eq!(switch["args"]["schedule_point"], "irq-tail");
    }
    Ok(())
}

#[test]
fn mismatched_nested_irq_boundaries_discard_outer_context() -> Result<(), Box<dyn std::error::Error>>
{
    for kind_mismatch in [false, true] {
        let mut c = capture();
        add(&mut c, 0, 2000, 0, "irq_enter", &[("irq", 26)]);
        add(&mut c, 0, 3000, 1, "irq_enter", &[("irq", 7)]);
        add(
            &mut c,
            0,
            4000,
            2,
            "irq_exit",
            &[
                ("irq", if kind_mismatch { 7 } else { 8 }),
                ("irq_kind", u64::from(kind_mismatch)),
            ],
        );
        add(&mut c, 0, 5000, 3, "sched_request", &[("reason", 1)]);
        add(&mut c, 0, 6000, 4, "irq_exit", &[("irq", 26)]);
        // A fresh complete interval must still be usable after resynchronizing.
        add(&mut c, 0, 7000, 5, "irq_enter", &[("irq", 30)]);
        add(&mut c, 0, 8000, 6, "irq_exit", &[("irq", 30)]);
        let value = json(&mut c)?;
        let intervals = spans(&value);
        assert_eq!(intervals.len(), 1);
        assert_eq!(intervals[0]["name"], "IRQ 30");
        let request = value["traceEvents"]
            .as_array()
            .ok_or("missing events")?
            .iter()
            .find(|e| e["name"] == "sched_request")
            .ok_or("missing request")?;
        assert!(request["args"]["irq_context"].is_null());
    }
    Ok(())
}

#[test]
fn reschedule_irq_context_requires_matching_local_complete_boundaries()
-> Result<(), Box<dyn std::error::Error>> {
    for mode in [
        "physical",
        "nested",
        "private",
        "missing-exit",
        "gap",
        "mismatch",
        "other-cpu",
    ] {
        let mut c = capture();
        let kind = u64::from(mode == "private");
        add(
            &mut c,
            0,
            2000,
            0,
            "irq_enter",
            &[("irq", 26), ("irq_kind", kind)],
        );
        let mut seq = 1;
        if mode == "nested" {
            add(&mut c, 0, 2500, seq, "irq_enter", &[("irq", 7)]);
            seq += 1;
        }
        let cpu = usize::from(mode == "other-cpu");
        add(
            &mut c,
            cpu,
            3000,
            if cpu == 0 { seq } else { 0 },
            "sched_request",
            &[("reason", 2), ("target_cpu", 1)],
        );
        seq += 1;
        if mode == "nested" {
            add(&mut c, 0, 3500, seq, "irq_exit", &[("irq", 7)]);
            seq += 1;
        }
        if mode != "missing-exit" {
            add(
                &mut c,
                0,
                4000,
                if mode == "gap" {
                    seq + 1
                } else if cpu == 1 {
                    1
                } else {
                    seq
                },
                "irq_exit",
                &[
                    ("irq", if mode == "mismatch" { 30 } else { 26 }),
                    ("irq_kind", kind),
                ],
            );
        }
        let value = json(&mut c)?;
        let event = value["traceEvents"]
            .as_array()
            .ok_or("missing events")?
            .iter()
            .find(|e| e["name"] == "sched_request")
            .ok_or("missing request")?;
        assert_eq!(event["args"]["request_source"], "scheduler-deadline");
        let context = &event["args"]["irq_context"];
        match mode {
            "physical" => assert_eq!(context["irq"], 26),
            "nested" => assert_eq!(context["irq"], 7),
            "private" => {
                assert_eq!(context["kind"], "kernel-rpc");
                assert!(context["irq"].is_null());
            }
            _ => assert!(context.is_null(), "{mode}"),
        }
    }
    Ok(())
}

fn block_event(c: &mut Capture, cpu: usize, time: u64, name: &str, fields: &[(&str, u64)]) {
    let sequence = c.events.iter().filter(|event| event.cpu == cpu).count() as u64;
    add(c, cpu, time, sequence, name, fields);
}

fn physical_command(c: &mut Capture, tag: u64, slot: u64, opcode: u64, start: u64, end: u64) {
    block_event(
        c,
        0,
        start,
        "block_submit",
        &[("domain", 7), ("tag", tag), ("bytes", 1024), ("read", 0)],
    );
    block_event(
        c,
        0,
        start + 1,
        "block_command",
        &[
            ("domain", 7),
            ("tag", tag),
            ("slot", slot),
            ("opcode", opcode),
        ],
    );
    block_event(
        c,
        0,
        end,
        "block_complete",
        &[("domain", 7), ("tag", tag), ("success", 1)],
    );
}

#[test]
fn block_many_to_many_dispatch_and_retained_completions() -> Result<(), Box<dyn std::error::Error>>
{
    let mut c = capture();
    for request in [1, 2] {
        block_event(
            &mut c,
            1,
            2000 + request,
            "block_queued",
            &[
                ("domain", 7),
                ("request", request),
                ("queue", 9),
                ("ticket", 0x100 + request),
            ],
        );
        block_event(
            &mut c,
            1,
            2010 + request,
            "block_origin",
            &[
                ("domain", 7),
                ("request", request),
                ("thread", 99),
                ("operation", 1),
            ],
        );
    }
    // Batch 1 contributes two fragments to command 1. Batch 2 spans both
    // concurrently outstanding commands. A logical ticket is not a DMA tag.
    physical_command(&mut c, 1, 0, 0x8a, 3000, 5000);
    physical_command(&mut c, 2, 1, 0x8a, 3100, 4500);
    for (tag, request, bytes, time) in [
        (1, 1, 256, 3010),
        (1, 1, 256, 3020),
        (1, 2, 512, 3030),
        (2, 2, 1024, 3110),
    ] {
        block_event(
            &mut c,
            0,
            time,
            "block_link",
            &[
                ("domain", 7),
                ("tag", tag),
                ("request", request),
                ("bytes", bytes),
            ],
        );
    }
    for request in [1, 2] {
        block_event(
            &mut c,
            1,
            5100 + request,
            "block_ready",
            &[
                ("domain", 7),
                ("request", request),
                ("status", 0),
                ("bytes", if request == 1 { 512 } else { 1536 }),
            ],
        );
        block_event(
            &mut c,
            1,
            6100 + request,
            "block_release",
            &[("domain", 7), ("request", request), ("queue", 9)],
        );
    }
    // Renumber in chronological order per CPU, as the real producer does.
    c.sort();
    for cpu in 0..2 {
        for (sequence, e) in c.events.iter_mut().filter(|e| e.cpu == cpu).enumerate() {
            e.sequence = sequence as u64;
        }
    }
    let value = json(&mut c)?;
    let physical: Vec<_> = spans(&value)
        .into_iter()
        .filter(|e| e["pid"] == 4 && e["name"] == "write")
        .collect();
    assert_eq!(physical.len(), 2);
    assert_ne!(physical[0]["tid"], physical[1]["tid"]);
    let dispatch: Vec<_> = flows(&value)
        .into_iter()
        .filter(|e| e["name"] == "Block dispatch")
        .collect();
    assert_eq!(dispatch.len(), 3);
    assert!(
        dispatch
            .iter()
            .any(|e| e["args"]["request"] == "0x1" && e["args"]["bytes"] == 512)
    );
    assert_eq!(
        flows(&value)
            .iter()
            .filter(|e| e["name"] == "Block completion")
            .count(),
        3
    );
    assert_eq!(
        spans(&value)
            .iter()
            .filter(|e| e["name"] == "Completion retained" && e["dur"] == 1.0)
            .count(),
        2
    );
    assert_eq!(
        spans(&value)
            .iter()
            .filter(|e| e["name"] == "Dispatch delay")
            .count(),
        2
    );
    let counts: Vec<_> = value["traceEvents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["ph"] == "C" && e["pid"] == 6)
        .map(|e| &e["args"]["in flight"])
        .collect();
    assert_eq!(
        counts,
        vec![
            &Value::from(1),
            &Value::from(2),
            &Value::from(1),
            &Value::from(0)
        ]
    );
    Ok(())
}

#[test]
fn block_equal_timestamps_do_not_depend_on_cpu_order() -> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    let device = (1u64 << 60) + 7;
    let request = (1u64 << 60) + 9;
    block_event(
        &mut c,
        1,
        2000,
        "block_queued",
        &[
            ("domain", device),
            ("request", request),
            ("queue", 1),
            ("ticket", 0x101),
        ],
    );
    block_event(
        &mut c,
        1,
        2000,
        "block_origin",
        &[
            ("domain", device),
            ("request", request),
            ("thread", 99),
            ("operation", 2),
        ],
    );
    block_event(
        &mut c,
        0,
        2000,
        "block_submit",
        &[("domain", device), ("tag", 1), ("bytes", 0), ("read", 0)],
    );
    block_event(
        &mut c,
        0,
        2000,
        "block_command",
        &[
            ("domain", device),
            ("tag", 1),
            ("slot", 0),
            ("opcode", 0x91),
        ],
    );
    block_event(
        &mut c,
        0,
        2000,
        "block_link",
        &[
            ("domain", device),
            ("tag", 1),
            ("request", request),
            ("bytes", 0),
        ],
    );
    block_event(
        &mut c,
        0,
        2000,
        "block_complete",
        &[("domain", device), ("tag", 1), ("success", 1)],
    );
    block_event(
        &mut c,
        1,
        2000,
        "block_ready",
        &[
            ("domain", device),
            ("request", request),
            ("status", 0),
            ("bytes", 0),
        ],
    );
    let value = json(&mut c)?;
    assert!(
        spans(&value)
            .iter()
            .any(|e| e["name"] == "flush" && e["dur"] == 0.0)
    );
    assert_eq!(
        flows(&value)
            .iter()
            .filter(|e| e["name"] == "Block dispatch")
            .count(),
        1
    );
    assert_eq!(
        flows(&value)
            .iter()
            .filter(|e| e["name"] == "Block completion")
            .count(),
        1
    );
    let counters: Vec<_> = value["traceEvents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["ph"] == "C")
        .collect();
    assert_eq!(counters[1]["args"]["in flight"], 0);
    let bytes = serde_json::to_vec(&value)?;
    if let Some(path) = std::env::var_os("HYPER_BLOCK_TRACE_FIXTURE") {
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

#[test]
fn block_loss_ambiguity_and_unmatched_boundaries_never_invent_commands()
-> Result<(), Box<dyn std::error::Error>> {
    for variant in 0..7 {
        let mut c = capture();
        physical_command(&mut c, 1, 0, 0x88, 2000, 4000);
        match variant {
            0 => c.losses[1].incomplete = true,
            1 => c.info.cpu_mask[0] = 1,
            2 => c.events[1].sequence = 9,
            3 => c.events[1].site = c.events[0].site, // duplicate begin, no slot metadata
            4 => {
                block_event(&mut c, 0, 5000, "block_abort", &[("domain", 7), ("tag", 1)]);
            }
            5 => c.events[2].time = 1500,
            _ => {
                c.events.pop();
            }
        }
        let value = json(&mut c)?;
        assert!(
            spans(&value).iter().all(|e| e["pid"] != 4),
            "variant {variant}"
        );
        assert!(flows(&value).is_empty());
        assert!(
            value["traceEvents"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|e| e["ph"] == "i" && e["name"] == "block_submit")
        );
    }
    Ok(())
}

#[test]
fn block_abort_and_cancelled_batch_are_not_successful_device_completions()
-> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    physical_command(&mut c, 1, 0, 0x8a, 2000, 4000);
    let site = c.events[2].site;
    c.sites[site].name = "block_abort".into();
    // Unrelated future records with the same tag must never consume a command.
    block_event(
        &mut c,
        1,
        3000,
        "future_diagnostic",
        &[("domain", 7), ("tag", 1)],
    );
    block_event(
        &mut c,
        1,
        5000,
        "block_queued",
        &[
            ("domain", 7),
            ("request", 2),
            ("queue", 9),
            ("ticket", 0x101),
        ],
    );
    block_event(
        &mut c,
        1,
        6000,
        "block_ready",
        &[
            ("domain", 7),
            ("request", 2),
            ("status", (-18i64) as u64),
            ("bytes", 0),
        ],
    );
    block_event(
        &mut c,
        1,
        7000,
        "block_release",
        &[("domain", 7), ("request", 2), ("queue", 9)],
    );
    let value = json(&mut c)?;
    assert!(spans(&value).iter().any(|e| e["name"] == "aborted command"));
    assert!(
        spans(&value)
            .iter()
            .any(|e| e["name"] == "Block batch" && e["args"]["completion"]["status"] == -18)
    );
    assert!(flows(&value).is_empty());
    Ok(())
}

#[test]
fn block_ticket_slot_reuse_preserves_identity_and_unknown_actor_is_not_a_caller()
-> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    for request in [1, 2] {
        let time = request * 4000;
        block_event(
            &mut c,
            0,
            time,
            "block_queued",
            &[
                ("domain", 7),
                ("request", request),
                ("queue", 9),
                ("ticket", (request << 8) | 1),
            ],
        );
        block_event(
            &mut c,
            0,
            time + 1,
            "block_origin",
            &[
                ("domain", 7),
                ("request", request),
                ("thread", 0),
                ("operation", 1),
            ],
        );
        block_event(
            &mut c,
            0,
            time + 1000,
            "block_ready",
            &[
                ("domain", 7),
                ("request", request),
                ("status", (-18i64) as u64),
                ("bytes", 0),
            ],
        );
        block_event(
            &mut c,
            0,
            time + 2000,
            "block_release",
            &[("domain", 7), ("request", request), ("queue", 9)],
        );
    }
    let value = json(&mut c)?;
    let batches: Vec<_> = spans(&value)
        .into_iter()
        .filter(|e| e["name"] == "Block batch")
        .collect();
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0]["tid"], batches[1]["tid"]);
    assert_ne!(batches[0]["args"]["request"], batches[1]["args"]["request"]);
    assert!(flows(&value).is_empty());
    Ok(())
}

#[test]
fn logical_block_tracks_survive_native_export_without_aliasing_scheduler_tracks()
-> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    block_event(
        &mut c,
        0,
        2000,
        "block_queued",
        &[
            ("domain", 7),
            ("request", 1),
            ("queue", 9),
            ("ticket", 0x101),
        ],
    );
    block_event(
        &mut c,
        0,
        3000,
        "block_ready",
        &[("domain", 7), ("request", 1), ("status", 0), ("bytes", 512)],
    );
    block_event(
        &mut c,
        0,
        4000,
        "block_release",
        &[("domain", 7), ("request", 1), ("queue", 9)],
    );
    let value = json(&mut c)?;
    for name in ["Block batch", "Completion retained"] {
        let span = spans(&value)
            .into_iter()
            .find(|event| event["name"] == name)
            .ok_or("missing block interval")?;
        assert_eq!(span["pid"], 7);
    }
    let mut native = Vec::new();
    export::perfetto(&c, &mut native, false)?;
    // The native writer discards synthetic scheduler tracks. Block intervals
    // must retain their own descriptors and spans in the default output format.
    for name in [
        "HypeR logical block batches",
        "Block batch",
        "Completion retained",
    ] {
        assert!(
            native
                .windows(name.len())
                .any(|bytes| bytes == name.as_bytes())
        );
    }
    Ok(())
}
