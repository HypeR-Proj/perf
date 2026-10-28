// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Local scheduling evidence. A request is not proof of a switch, and a nearby
//! interrupt is not proof of its cause. Never join across missing records.

use super::{Capture, Event};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub(super) fn describe(c: &Capture, e: &Event, args: &mut Map<String, Value>) {
    match c.sites[e.site].name.as_str() {
        "sched_switch" => {
            let cause = c.value(e, "cause");
            let reason = cause.map(|value| value & 0xff);
            let point = cause.map(|value| (value >> 8) & 0xff);
            args.insert("prev_state".into(), json!(previous_state(c, e)));
            args.insert("next_class".into(), json!(next_class(c, e)));
            args.insert("reason".into(), json!(reason));
            args.insert("point".into(), json!(point));
            args.insert(
                "switch_reason".into(),
                json!(match reason {
                    Some(1) => "yield",
                    Some(2) => "preempt",
                    Some(3) => "block",
                    Some(4) => "exit",
                    Some(5) => "migrate",
                    Some(6) => "idle-dispatch",
                    _ => "unknown",
                }),
            );
            args.insert(
                "preempted".into(),
                match reason {
                    Some(2) => json!(true),
                    // A pending migration can also be committed by yield_now.
                    // Preserve the placement reason without calling that
                    // explicitly voluntary scheduling point a preemption.
                    Some(5) => match point {
                        Some(1) => json!(false),
                        Some(2 | 3) => json!(true),
                        _ => Value::Null,
                    },
                    Some(1 | 3 | 4 | 6) => json!(false),
                    _ => Value::Null,
                },
            );
            args.insert(
                "schedule_point".into(),
                json!(match point {
                    Some(1) => "explicit",
                    Some(2) => "safe-point",
                    Some(3) => "irq-tail",
                    _ => "unknown",
                }),
            );
        }
        "sched_request" => {
            args.insert(
                "request_source".into(),
                json!(match c.value(e, "reason") {
                    Some(1) => "wakeup",
                    Some(2) => "scheduler-deadline",
                    Some(3) => "stop",
                    Some(4) => "policy-change",
                    Some(5) => "migration",
                    _ => "unknown",
                }),
            );
        }
        _ => {}
    }
}

pub(super) fn previous_state(c: &Capture, e: &Event) -> Option<u64> {
    c.value(e, "state").map(|state| state & 0xff)
}

pub(super) fn next_class(c: &Capture, e: &Event) -> Option<u64> {
    c.value(e, "state").map(|state| (state >> 8) & 0xff)
}

struct IrqFrame<'a> {
    begin: &'a Event,
    requests: Vec<&'a Event>,
}

#[derive(Default)]
pub(super) struct Evidence<'a> {
    pub request_irqs: BTreeMap<(usize, u64), &'a Event>,
}

impl<'a> Evidence<'a> {
    pub fn new(c: &'a Capture) -> Self {
        let mut result = Self::default();
        let count = c.info.cpu_count as usize;
        let mut previous: Vec<Option<&Event>> = vec![None; count];
        let mut irq: Vec<Vec<IrqFrame<'a>>> = (0..count).map(|_| Vec::new()).collect();
        for e in &c.events {
            let last = previous[e.cpu].replace(e);
            let consecutive = last.is_some_and(|p| p.sequence.checked_add(1) == Some(e.sequence));
            if !consecutive {
                irq[e.cpu].clear();
            }
            let name = c.sites[e.site].name.as_str();
            if c.losses[e.cpu].incomplete {
                continue;
            }
            match name {
                "irq_enter" => irq[e.cpu].push(IrqFrame {
                    begin: e,
                    requests: Vec::new(),
                }),
                "sched_request" => {
                    if let Some(frame) = irq[e.cpu].last_mut() {
                        frame.requests.push(e);
                    }
                }
                "irq_exit" => {
                    if let Some(frame) = irq[e.cpu].pop()
                        && c.value(frame.begin, "irq").is_some()
                        && c.value(frame.begin, "irq") == c.value(e, "irq")
                        && c.value(frame.begin, "irq_kind").unwrap_or(0)
                            == c.value(e, "irq_kind").unwrap_or(0)
                    {
                        for request in frame.requests {
                            result
                                .request_irqs
                                .insert((e.cpu, request.sequence), frame.begin);
                        }
                    } else {
                        irq[e.cpu].clear();
                    }
                }
                _ => {}
            }
        }
        result
    }
}

pub(super) fn irq_context(c: &Capture, e: &Event) -> Value {
    match c.value(e, "irq_kind").unwrap_or(0) {
        0 => json!({"kind":"physical", "irq":c.value(e,"irq"), "enter_sequence":e.sequence}),
        1 => json!({"kind":"kernel-rpc", "irq":null, "enter_sequence":e.sequence}),
        kind => json!({"kind":"unknown", "raw_kind":kind, "irq":c.value(e,"irq")}),
    }
}
