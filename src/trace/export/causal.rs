// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Only explicit, matching commit identities establish causal edges. No payload
//! parsing or inference from temporal proximity / a thread's previous message.

use super::{Capture, Event, TraceWriter, fields};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
};

type Key = (u64, u64);
type MessageKey = (bool, u64, u64);

#[derive(Default)]
pub(super) struct Links<'capture> {
    sent_messages: BTreeMap<MessageKey, &'capture Event>,
    received_messages: BTreeMap<MessageKey, &'capture Event>,
    blocked: BTreeMap<Key, &'capture Event>,
    waking: BTreeMap<Key, &'capture Event>,
    runnable: BTreeMap<u64, &'capture Event>,
    clients: BTreeMap<Key, &'capture Event>,
    servers: BTreeMap<Key, &'capture Event>,
    ambiguous: BTreeSet<Key>,
    next_flow: u64,
}

impl<'capture> Links<'capture> {
    pub(super) fn new(c: &Capture) -> Self {
        let mut links = Self::default();
        let mut clients = BTreeSet::new();
        let mut servers = BTreeSet::new();
        // An overlap discovered after a dispatch also invalidates that earlier
        // edge. Find ambiguous user annotations before emitting any JSON.
        for e in &c.events {
            let name = c.sites[e.site].name.as_str();
            let (Some(connection), Some(request)) =
                (c.value(e, "connection"), c.value(e, "request"))
            else {
                continue;
            };
            let key = (connection, request);
            let active = match name {
                "rpc_client_begin" | "rpc_client_end" => &mut clients,
                "rpc_server_begin" | "rpc_server_end" => &mut servers,
                _ => continue,
            };
            if name.ends_with("_begin") {
                if !active.insert(key) {
                    links.ambiguous.insert(key);
                }
            } else {
                active.remove(&key);
            }
        }
        links
    }

    pub(super) fn clear(&mut self) {
        let next_flow = self.next_flow;
        let ambiguous = std::mem::take(&mut self.ambiguous);
        *self = Self::default();
        self.next_flow = next_flow;
        self.ambiguous = ambiguous;
    }

    pub(super) fn observe(
        &mut self,
        c: &Capture,
        e: &'capture Event,
        threads: &BTreeMap<u64, u32>,
        w: &mut TraceWriter<impl Write>,
    ) -> io::Result<()> {
        self.observe_message(c, e, threads, w)?;
        // Queue identity and non-reused commit sequence establish delivery
        // independently of unrelated lost observations. RPC user annotations
        // and wait lifetimes need the stronger complete-capture contract.
        if !c.cross_cpu_spans() {
            return Ok(());
        }
        self.observe_wait(c, e, threads, w)?;
        self.observe_rpc(c, e, threads, w)
    }

    fn observe_message(
        &mut self,
        c: &Capture,
        e: &'capture Event,
        threads: &BTreeMap<u64, u32>,
        w: &mut TraceWriter<impl Write>,
    ) -> io::Result<()> {
        let name = c.sites[e.site].name.as_str();
        if let (Some(queue), Some(message)) = (c.value(e, "queue"), c.value(e, "message"))
            && queue != 0
            && message != 0
        {
            let key = (name.starts_with("cap_"), queue, message);
            match name {
                "ipc_enqueue" | "cap_enqueue" => {
                    // Equal timestamps are sorted by CPU, which can visit a
                    // committed receive first. Match its identity in either
                    // traversal order; edge() still rejects negative latency.
                    if let Some(receive) = self.received_messages.remove(&key) {
                        self.edge(c, e, receive, "Message delivery", threads, w)?;
                    } else {
                        self.sent_messages.insert(key, e);
                    }
                }
                "ipc_dequeue" | "cap_dequeue" => {
                    if let Some(send) = self.sent_messages.remove(&key) {
                        self.edge(c, send, e, "Message delivery", threads, w)?;
                    } else {
                        self.received_messages.insert(key, e);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn observe_wait(
        &mut self,
        c: &Capture,
        e: &'capture Event,
        threads: &BTreeMap<u64, u32>,
        w: &mut TraceWriter<impl Write>,
    ) -> io::Result<()> {
        let name = c.sites[e.site].name.as_str();
        if let (Some(thread), Some(wait)) = (c.value(e, "thread"), c.value(e, "wait")) {
            let key = (thread, wait);
            match name {
                "wait_block" => {
                    self.blocked.insert(key, e);
                }
                "wait_complete" => {
                    if let Some(start) = self.blocked.remove(&key)
                        && let Some(tid) = threads.get(&thread)
                    {
                        let reason = match c.value(e, "outcome") {
                            Some(1) => "Blocked (notified)",
                            Some(2) => "Blocked (timeout)",
                            _ => "Blocked (cancelled)",
                        };
                        let mut args = fields(c, start);
                        args["completion"] = fields(c, e);
                        w.span((2, *tid), reason, "scheduler", start.time..e.time, args)?;
                    }
                }
                "signal_wake" => {
                    self.waking.insert(key, e);
                }
                "sched_wakeup" => {
                    if let Some(wake) = self.waking.remove(&key) {
                        self.edge(c, wake, e, "Signal notification", threads, w)?;
                    }
                    self.runnable.insert(thread, e);
                }
                _ => {}
            }
        }
        if name == "sched_switch"
            && let Some(next) = c.value(e, "next")
            && let Some(start) = self.runnable.remove(&next)
        {
            self.edge(c, start, e, "Wake to execution", threads, w)?;
        }
        Ok(())
    }

    fn observe_rpc(
        &mut self,
        c: &Capture,
        e: &'capture Event,
        threads: &BTreeMap<u64, u32>,
        w: &mut TraceWriter<impl Write>,
    ) -> io::Result<()> {
        let name = c.sites[e.site].name.as_str();
        if let (Some(connection), Some(request)) = (c.value(e, "connection"), c.value(e, "request"))
            && connection != 0
            && request != 0
        {
            let key = (connection, request);
            match name {
                "rpc_client_begin" | "rpc_server_begin" => {
                    let table = if name == "rpc_client_begin" {
                        &mut self.clients
                    } else {
                        &mut self.servers
                    };
                    table.insert(key, e);
                    if name == "rpc_server_begin"
                        && !self.ambiguous.contains(&key)
                        && let Some(client) = self.clients.get(&key).copied()
                        && c.value(client, "operation") == c.value(e, "operation")
                    {
                        self.edge(c, client, e, "RPC dispatch", threads, w)?;
                    }
                }
                "rpc_client_end" | "rpc_server_end" => {
                    let client = name == "rpc_client_end";
                    let start = if client {
                        self.clients.remove(&key)
                    } else {
                        self.servers.remove(&key)
                    };
                    if !self.ambiguous.contains(&key)
                        && let Some(start) = start
                        && c.value(start, "thread") == c.value(e, "thread")
                        && let Some(tid) = c.value(e, "thread").and_then(|id| threads.get(&id))
                    {
                        let mut args = fields(c, start);
                        args["completion"] = fields(c, e);
                        w.span(
                            (2, *tid),
                            if client {
                                "RPC round trip"
                            } else {
                                "RPC handler"
                            },
                            "ipc",
                            start.time..e.time,
                            args,
                        )?;
                    }
                }
                "rpc_parent" if !self.ambiguous.contains(&key) => {
                    if let (Some(connection), Some(request)) = (
                        c.value(e, "parent_connection"),
                        c.value(e, "parent_request"),
                    ) {
                        let parent = (connection, request);
                        if parent != key
                            && !self.ambiguous.contains(&parent)
                            && let Some(start) = self
                                .servers
                                .get(&parent)
                                .or_else(|| self.clients.get(&parent))
                                .copied()
                            && let Some(child) = self
                                .clients
                                .get(&key)
                                .or_else(|| self.servers.get(&key))
                                .copied()
                        {
                            self.edge(c, start, child, "RPC parent", threads, w)?;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn edge(
        &mut self,
        c: &Capture,
        start: &Event,
        end: &Event,
        name: &str,
        threads: &BTreeMap<u64, u32>,
        w: &mut TraceWriter<impl Write>,
    ) -> io::Result<()> {
        let Some(thread) = c.value(end, "thread").or_else(|| c.value(end, "next")) else {
            return Ok(());
        };
        let Some(from) = c
            .value(start, "thread")
            .filter(|id| *id != 0)
            .and_then(|id| threads.get(&id))
        else {
            return Ok(());
        };
        let Some(to) = threads.get(&thread).filter(|_| thread != 0) else {
            return Ok(());
        };
        if end.time < start.time {
            return Ok(());
        }
        self.next_flow += 1; // At most a small multiple of the bounded capture.
        w.anchor(*from, start, name, self.next_flow, true, fields(c, start))?;
        let mut args = fields(c, end);
        args["latency_ns"] = json!(end.time - start.time);
        w.anchor(*to, end, name, self.next_flow, false, args)
    }
}
