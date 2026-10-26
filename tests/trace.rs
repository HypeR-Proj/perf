// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0
use crate::{export, model, options};
use hyper_os::inspect::trace::TraceInfo;
use model::{Capture, Event, Site};
use serde_json::Value;
fn capture() -> Capture {
    Capture::new(
        TraceInfo {
            generation: 1,
            started_ns: 1000,
            deadline_ns: 1_000_000,
            categories: 127,
            cpu_count: 2,
            capacity: 64,
            maximum_capacity: 4096,
            running: 0,
            cpu_mask: core::array::from_fn(|i| if i == 0 { 3 } else { 0 }),
            revision: [0; 64],
        },
        128,
    )
}
fn add(c: &mut Capture, cpu: usize, time: u64, sequence: u64, name: &str, fields: &[(&str, u64)]) {
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
    c.sort();
    let mut bytes = Vec::new();
    export::perfetto(c, &mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
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
    assert!(flows(&json(&mut c)?).is_empty());
    c.losses[1].overwritten = 0;
    c.info.cpu_mask[0] = 1;
    assert!(flows(&json(&mut c)?).is_empty());
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
                assert!(flows(&json(&mut c)?).is_empty());
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
        &[("prev", 30), ("next", 10), ("prev_state", 4)],
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
fn unknown_loss_disables_inferred_durations() -> Result<(), Box<dyn std::error::Error>> {
    let mut c = capture();
    c.losses[0].incomplete = true;
    add(
        &mut c,
        0,
        2000,
        0,
        "sched_switch",
        &[("prev", 1), ("next", 2), ("prev_state", 1)],
    );
    add(
        &mut c,
        0,
        4000,
        1,
        "sched_switch",
        &[("prev", 2), ("next", 1), ("prev_state", 4)],
    );
    let value = json(&mut c)?;
    assert!(spans(&value).is_empty());
    assert_eq!(value["metadata"]["losses"][0]["incomplete"], true);
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
        &[("prev", 1), ("next", 2), ("prev_state", 4)],
    );
    add(
        &mut c,
        0,
        6000,
        1,
        "sched_switch",
        &[("prev", 2), ("next", 1), ("prev_state", 4)],
    );
    let value = json(&mut c)?;
    let items = spans(&value);
    assert_eq!(items.len(), 2);
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
            assert!(export.no_names && export.fail_on_loss);
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
        4000,
        1,
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
