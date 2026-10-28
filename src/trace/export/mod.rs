// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0
//! Chrome Trace Event JSON accepted by Perfetto. Every derived span requires
//! observed matching boundaries. Gaps invalidate pending spans. Scheduling
//! windows remain visible; original capture metadata retains loss information.
use super::model::{Capture, Event};
mod block;
mod causal;
mod proto;
mod scheduling;
mod timeline;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, Write},
};

pub fn jsonl(capture: &Capture, mut output: impl Write) -> io::Result<()> {
    writeln!(output, "{}", metadata(capture))?;
    for event in &capture.events {
        let site = &capture.sites[event.site];
        writeln!(
            output,
            "{}",
            json!({"cpu":event.cpu,"sequence":event.sequence,"time_ns":event.time,
            "category":site.category,"event":site.name,"source":site.source,"line":site.line,"args":fields(capture,event)})
        )?;
    }
    writeln!(
        output,
        "{}",
        json!({"end":true,"events":capture.events.len(),"loss":capture.loss(),"limited":capture.limited})
    )
}
fn metadata(c: &Capture) -> Value {
    let revision = String::from_utf8_lossy(&c.info.revision)
        .trim_end_matches('\0')
        .to_string();
    let losses: Vec<_> = c
        .losses
        .iter()
        .map(|l| json!({"cpu":l.cpu,"overwritten":l.overwritten,"incomplete":l.incomplete}))
        .collect();
    let names: BTreeMap<String, &String> = c
        .names
        .iter()
        .map(|(id, name)| (format!("{id:#x}"), name))
        .collect();
    let objects: BTreeMap<_, _> = c
        .koids
        .iter()
        .map(|(id, koid)| {
            (
                format!("{id:#x}"),
                json!({"koid":format!("{koid:#x}"),
            "process_koid":c.tasks.get(id).and_then(|task|task.process).map(|id|format!("{id:#x}"))}),
            )
        })
        .collect();
    json!({"format":"hyper-trace-v1","kernel":revision,"generation":c.info.generation,"clock":"monotonic-ns",
        "started_ns":c.info.started_ns,"deadline_ns":c.info.deadline_ns,"categories":c.info.categories,
        "capacity_per_cpu":c.info.capacity,"cpu_count":c.info.cpu_count,"cpu_mask":c.info.cpu_mask, "losses":losses,
        "vm_threads":c.vms.iter().map(|(id, vm)| (format!("{id:#x}"), json!({"name":vm.name,"vcpu":vm.vcpu}))).collect::<BTreeMap<_,_>>(),
        "thread_names":names,"thread_objects":objects,"limited":c.limited,"cross_cpu_spans":c.cross_cpu_spans()})
}
fn fields(c: &Capture, event: &Event) -> Value {
    let mut args = serde_json::Map::new();
    for (name, value) in c.sites[event.site].fields.iter().zip(event.values) {
        // Preserve all bits of IDs in JavaScript readers, even above 2^53.
        args.insert(
            name.clone(),
            if matches!(
                name.as_str(),
                "thread"
                    | "koid"
                    | "prev"
                    | "next"
                    | "waker"
                    | "process"
                    | "channel"
                    | "vm"
                    | "domain"
                    | "tag"
                    | "ticket"
                    | "queue"
                    | "message"
                    | "connection"
                    | "request"
                    | "source_id"
                    | "wait"
                    | "parent_connection"
                    | "parent_request"
            ) {
                json!(format!("{value:#x}"))
            } else if name == "status" {
                json!(value as i64)
            } else {
                json!(value)
            },
        );
    }
    scheduling::describe(c, event, &mut args);
    args.insert("sequence".into(), json!(event.sequence));
    args.insert("cpu".into(), json!(event.cpu));
    args.insert("source".into(), json!(c.sites[event.site].source));
    args.insert("line".into(), json!(c.sites[event.site].line));
    Value::Object(args)
}
struct TraceWriter<W> {
    output: W,
    first: bool,
    origin: u64,
    native: Option<proto::Output>,
}
impl<W: Write> TraceWriter<W> {
    fn event(&mut self, value: Value) -> io::Result<()> {
        if let Some(native) = self.native.as_mut() {
            return native.event(value);
        }
        if !self.first {
            self.output.write_all(b",\n")?;
        }
        self.first = false;
        serde_json::to_writer(&mut self.output, &value).map_err(io::Error::other)
    }
    fn name(&mut self, pid: u32, tid: u32, name: &str) -> io::Result<()> {
        self.event(json!({"ph":"M","pid":pid,"tid":tid,"name":if tid==0 {"process_name"} else {"thread_name"},"args":{"name":name}}))
    }
    fn time(&self, ns: u64) -> f64 {
        ns.saturating_sub(self.origin) as f64 / 1000.0
    }
    fn span(
        &mut self,
        track: (u32, u32),
        name: &str,
        category: &str,
        time: std::ops::Range<u64>,
        args: Value,
    ) -> io::Result<()> {
        let (pid, tid) = track;
        let (begin, end) = (time.start, time.end);
        if end < begin {
            return Ok(());
        }
        self.event(json!({"ph":"X","pid":pid,"tid":tid,"name":name,"cat":category,"ts":self.time(begin),"dur":(end-begin) as f64 / 1000.0,"args":args}))
    }

    /// Explicit zero-duration slices let Perfetto bind arrows even when there
    /// is no enclosing syscall (kernel-originated messages and wakeups).
    fn anchor(
        &mut self,
        tid: u32,
        event: &Event,
        name: &str,
        id: u64,
        outgoing: bool,
        args: Value,
    ) -> io::Result<()> {
        self.anchor_track((2, tid, "ipc"), event, name, id, outgoing, args)
    }

    fn anchor_track(
        &mut self,
        track: (u32, u32, &str),
        event: &Event,
        name: &str,
        id: u64,
        outgoing: bool,
        args: Value,
    ) -> io::Result<()> {
        self.event(json!({"ph":"X","dur":0,"pid":track.0,"tid":track.1,"name":name,"cat":track.2,
            "ts":self.time(event.time),"bind_id":format!("{id:#x}"),"flow_out":outgoing,"flow_in":!outgoing,"args":args}))
    }
}
/// CPU and thread execution lanes represent bounded scheduling observations.
/// Activity lanes show elapsed syscall/VM/fault time and runnable latency.
/// These are native tracks, not fabricated Linux `sched_switch` records.
pub fn chrome(c: &Capture, output: impl Write, raw_events: bool) -> io::Result<()> {
    render(c, output, raw_events, false)
}
/// Native Perfetto protobuf populates the scheduler and thread-state tables.
pub fn perfetto(c: &Capture, output: impl Write, raw_events: bool) -> io::Result<()> {
    render(c, output, raw_events, true)
}
fn render(c: &Capture, output: impl Write, raw_events: bool, native: bool) -> io::Result<()> {
    let mut threads = BTreeMap::new();
    for event in &c.events {
        for key in ["thread", "prev", "next", "waker"] {
            if let Some(id) = c.value(event, key)
                && (id != 0
                    || matches!(key, "prev" | "next")
                    || key == "thread" && c.sites[event.site].name == "sched_reap")
                && !threads.contains_key(&id)
            {
                let tid = threads.len() as u32 + 1;
                threads.insert(id, tid);
            }
        }
    }
    let mut w = TraceWriter {
        output,
        first: true,
        origin: c.info.started_ns,
        native: if native {
            Some(proto::Output::new(c, &threads, raw_events)?)
        } else {
            None
        },
    };
    if !native {
        w.output.write_all(b"{\"traceEvents\":[\n")?;
    }
    w.name(1, 0, "HypeR CPU scheduling")?;
    w.name(2, 0, "HypeR threads (elapsed time)")?;
    w.name(3, 0, "HypeR IRQ service")?;
    w.name(4, 0, "HypeR block transport commands")?;
    w.name(7, 0, "HypeR logical block batches")?;
    w.name(6, 0, "HypeR captured block concurrency")?;
    for cpu in 0..c.info.cpu_count as u32 {
        w.name(1, cpu + 1, &format!("CPU {cpu}"))?;
        w.name(3, cpu + 1, &format!("CPU {cpu} IRQ"))?;
    }
    // Export-local small IDs avoid truncating the kernel's 64-bit identifiers.
    for (&id, &tid) in &threads {
        w.name(
            2,
            tid,
            &c.names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| format!("thread {id:#x}")),
        )?;
    }
    let mut previous_sequence = vec![None; c.info.cpu_count as usize];
    let mut pending: BTreeMap<(&str, u64), &Event> = BTreeMap::new();
    let mut ready: BTreeMap<u64, &Event> = BTreeMap::new();
    let mut irq: BTreeMap<usize, Vec<&Event>> = BTreeMap::new();
    let uncertain = !c.cross_cpu_spans();
    let scheduling = if raw_events {
        scheduling::Evidence::new(c)
    } else {
        scheduling::Evidence::default()
    };
    let mut causal = causal::Links::new(c);
    for event in &c.events {
        let site = &c.sites[event.site];
        let gap = previous_sequence[event.cpu]
            .is_some_and(|n: u64| n.checked_add(1) != Some(event.sequence));
        previous_sequence[event.cpu] = Some(event.sequence);
        if gap {
            // A migrated Thread or request completion may live on another CPU.
            pending.clear();
            ready.clear();
            irq.clear();
            causal.clear();
        }
        let thread = c.value(event, "thread");
        let tid = thread.and_then(|id| threads.get(&id).copied());
        if raw_events {
            let mut args = fields(c, event);
            if site.name == "sched_request" {
                args["irq_context"] = scheduling
                    .request_irqs
                    .get(&(event.cpu, event.sequence))
                    .map_or(Value::Null, |irq| scheduling::irq_context(c, irq));
            }
            w.event(json!({"ph":"i","s":"t","pid":if tid.is_some(){2}else{1},"tid":tid.unwrap_or(event.cpu as u32+1),
                "name":site.name,"cat":site.category,"ts":w.time(event.time),"args":args}))?;
        }
        if gap && raw_events {
            w.event(json!({"ph":"i","s":"t","pid":1,"tid":event.cpu+1,"name":"TRACE GAP","ts":w.time(event.time),"args":{"sequence":event.sequence}}))?;
        }
        causal.observe(c, event, &threads, &mut w)?;
        if site.name == "sched_switch" {
            if let (Some(prev), Some(next)) = (c.value(event, "prev"), c.value(event, "next")) {
                if !uncertain
                    && let Some(begin) = ready.remove(&next)
                    && let Some(tid) = threads.get(&next)
                {
                    w.span(
                        (2, *tid),
                        "Runnable",
                        "scheduler",
                        begin.time..event.time,
                        fields(c, begin),
                    )?;
                }
                if scheduling::previous_state(c, event) == Some(1) {
                    ready.entry(prev).or_insert(event);
                }
            }
        } else if site.name == "sched_wakeup"
            && let Some(thread) = thread
        {
            ready.entry(thread).or_insert(event);
        }
        if !uncertain && let Some(thread) = thread {
            let group = match site.name.as_str() {
                "syscall_enter" | "syscall_exit" => Some(("syscall", site.name == "syscall_enter")),
                "vcpu_enter" | "vcpu_exit" => Some(("vCPU run", site.name == "vcpu_enter")),
                "page_fault_begin" | "page_fault_end" => {
                    Some(("page fault", site.name == "page_fault_begin"))
                }
                _ => None,
            };
            if let Some((name, begin)) = group {
                if begin {
                    pending.insert((name, thread), event);
                } else if let Some(start) = pending.remove(&(name, thread))
                    && let Some(tid) = tid
                {
                    let mut args = fields(c, start);
                    args["completion"] = fields(c, event);
                    w.span((2, tid), name, &site.category, start.time..event.time, args)?;
                }
            }
        }
        if !c.losses[event.cpu].incomplete && site.name == "irq_enter" {
            irq.entry(event.cpu).or_default().push(event);
        }
        if !c.losses[event.cpu].incomplete && site.name == "irq_exit" {
            let stack = irq.entry(event.cpu).or_default();
            if let Some(start) = stack.pop()
                && c.value(start, "irq").is_some()
                && c.value(start, "irq") == c.value(event, "irq")
                && c.value(start, "irq_kind").unwrap_or(0)
                    == c.value(event, "irq_kind").unwrap_or(0)
            {
                w.span(
                    (3, event.cpu as u32 + 1),
                    &if c.value(start, "irq_kind") == Some(1) {
                        "Kernel RPC".into()
                    } else {
                        format!("IRQ {}", c.value(event, "irq").unwrap_or(0))
                    },
                    "irq",
                    start.time..event.time,
                    fields(c, start),
                )?;
            } else {
                // The remaining nesting is no longer trustworthy. Match the
                // conservative rule used for request IRQ attribution.
                stack.clear();
            }
        }
    }
    block::export(c, &threads, &mut w)?;

    if !native {
        timeline::write(c, &threads, &mut w)?;
    }
    // Deliberately do not close a running slice at the capture deadline: a final
    // switch/completion may never have been observed.
    for loss in &c.losses {
        if raw_events && (loss.overwritten != 0 || loss.incomplete) {
            w.event(
                json!({"ph":"i","s":"t","pid":1,"tid":loss.cpu+1,"name":"TRACE LOSS","ts":0,
                "args":{"overwritten":loss.overwritten,"incomplete":loss.incomplete}}),
            )?;
        }
    }
    if let Some(native) = w.native.take() {
        return native.finish(&mut w.output);
    }
    w.output
        .write_all(b"\n],\"displayTimeUnit\":\"ns\",\"metadata\":")?;
    serde_json::to_writer(&mut w.output, &metadata(c)).map_err(io::Error::other)?;
    w.output.write_all(b"}\n")
}
