// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0
use hyper_os::{
    Error, Status,
    inspect::{CpuInspector, ScanCursor, TaskInspector, ThreadRole},
};
use hyper_perf::{export, model, options};
pub use options::Command;
use std::{
    io::{self, Write},
    time::Duration,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn stop(inspector: &CpuInspector, generation: u64) -> Result<()> {
    for _ in 0..100 {
        match inspector.trace_stop(generation) {
            Ok(()) => return Ok(()),
            Err(Error::Status(Status::BUSY)) => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => return Err(error.into()),
        }
    }
    Err("trace admission is closed but a writer has not retired; retry stop".into())
}
fn writer(options: &options::Export) -> Result<Box<dyn Write>> {
    if options.output == "-" {
        return Ok(Box::new(io::stdout()));
    }
    let mut file = std::fs::OpenOptions::new();
    file.write(true);
    if options.overwrite {
        file.create(true).truncate(true);
    } else {
        file.create_new(true);
    }
    Ok(Box::new(file.open(&options.output)?))
}
fn collect(
    inspector: &CpuInspector,
    tasks: Option<&TaskInspector>,
    capture: &mut model::Capture,
    cursors: &mut [u64],
    names: bool,
) -> Result<()> {
    let mut unnamed = std::collections::BTreeSet::new();
    'cpus: for (cpu, cursor) in cursors.iter_mut().enumerate() {
        if capture.info.cpu_mask[cpu / 8] & (1 << (cpu % 8)) == 0 {
            continue;
        }
        // A bounded pass prevents one busy CPU from starving the other streams.
        for _ in 0..capture.info.capacity.div_ceil(8) {
            let batch = match inspector.trace_read(capture.info.generation, cpu as u64, *cursor) {
                Ok(batch) => batch,
                Err(Error::Status(Status::BUSY)) => break,
                Err(error) => return Err(error.into()),
            };
            capture.losses[cpu].overwritten += batch.overwritten();
            capture.losses[cpu].incomplete |= batch.incomplete();
            *cursor = batch.next();
            for record in batch.records() {
                let record = record?;
                if names {
                    for key in ["thread", "prev", "next", "waker"] {
                        if let Some(id) = record.value(key)
                            && (id != 0 || matches!(key, "prev" | "next"))
                            && !capture.names.contains_key(&id)
                        {
                            unnamed.insert(id);
                        }
                    }
                }
                if !capture.add(cpu, record)? {
                    break 'cpus;
                }
            }
            if batch.count() < 8 {
                break;
            }
        }
    }
    // Drain all rings before metadata lookups. Resolve each new identity once;
    // process/thread inspection is optional and never scheduler authority.
    if !unnamed.is_empty() {
        let mut objects = std::collections::BTreeMap::new();
        for id in unnamed {
            if let Ok(details) = inspector.trace_thread_details(capture.info.generation, id) {
                let koid = details.koid().get();
                capture.names.insert(id, details.name().to_owned());
                capture.koids.insert(id, koid);
                objects.insert(koid, id);
            } else if !capture.names.contains_key(&id) {
                let name = inspector
                    .trace_thread_name(capture.info.generation, id)
                    .map(|name| name.as_str().to_owned())
                    .unwrap_or_else(|_| format!("thread {id:#x}"));
                capture.names.insert(id, name);
            }
        }
        if let Some(tasks) = tasks
            && !objects.is_empty()
        {
            let _ = task_metadata(tasks, capture, &objects);
        }
    }
    Ok(())
}
fn task_metadata(
    inspector: &TaskInspector,
    capture: &mut model::Capture,
    objects: &std::collections::BTreeMap<u64, u64>,
) -> Result<()> {
    let mut cursor = Some(ScanCursor::START);
    while let Some(position) = cursor {
        let page = inspector.scan_threads(position)?;
        for task in page.entries() {
            if let Some(&id) = objects.get(&task.koid.get()) {
                capture.names.insert(id, task.name.as_str().to_owned());
                if task.role == ThreadRole::Vcpu {
                    capture.vcpu_threads.insert(id);
                }
                capture.tasks.insert(
                    id,
                    model::Task {
                        process: task.process_koid.map(|id| id.get()),
                        idle: task.role == ThreadRole::Idle,
                    },
                );
            }
        }
        cursor = page.next();
    }
    let mut cursor = Some(ScanCursor::START);
    while let Some(position) = cursor {
        let page = inspector.scan_processes(position)?;
        for process in page.entries() {
            capture
                .processes
                .insert(process.koid.get(), process.name.as_str().to_owned());
        }
        cursor = page.next();
    }
    Ok(())
}
fn write(
    capture: &mut model::Capture,
    options: &options::Export,
    output: Box<dyn Write>,
) -> Result<()> {
    capture.sort();
    let mut output = io::BufWriter::new(output);
    match options.format {
        options::Format::Perfetto => export::perfetto(capture, &mut output, options.raw_events)?,
        options::Format::Chrome => export::chrome(capture, &mut output, options.raw_events)?,
        options::Format::Jsonl => export::jsonl(capture, &mut output)?,
    }
    output.flush()?;
    let lost: u64 = capture.losses.iter().map(|loss| loss.overwritten).sum();
    eprintln!(
        "perf trace: {} events; {lost} overwritten; incomplete CPUs={}; event limit reached={}",
        capture.events.len(),
        capture.losses.iter().filter(|loss| loss.incomplete).count(),
        capture.limited
    );
    if options.fail_on_loss && capture.loss() {
        return Err("trace contains loss; output retained for diagnosis".into());
    }
    Ok(())
}
fn require_generation(
    inspector: &CpuInspector,
    expected: Option<u64>,
) -> Result<hyper_os::inspect::trace::TraceInfo> {
    let info = inspector.trace_info()?;
    if info.generation == 0 {
        return Err("no trace capture has been started".into());
    }
    if expected.is_some_and(|generation| generation != info.generation) {
        return Err("trace capture generation changed".into());
    }
    Ok(info)
}
mod machines;

pub fn run(
    command: Command,
    inspector: &CpuInspector,
    tasks: Option<&TaskInspector>,
    observer: Option<hyper_os::OwnedHandle<hyper_os::handle::ByteChannelObject>>,
) -> Result<std::process::ExitCode> {
    let mut machines = machines::Observer::new(observer);
    match command {
        Command::Status => {
            let i = inspector.trace_info()?;
            let cpus: Vec<_> = (0..i.cpu_count as usize)
                .filter(|cpu| i.cpu_mask[cpu / 8] & (1 << (cpu % 8)) != 0)
                .collect();
            println!(
                "generation={} running={} categories={:#x} CPUs={cpus:?}\nstart={} ns deadline={} ns retained={}/CPU maximum={}/CPU\nkernel={}",
                i.generation,
                i.running != 0,
                i.categories,
                i.started_ns,
                i.deadline_ns,
                i.capacity,
                i.maximum_capacity,
                String::from_utf8_lossy(&i.revision).trim_end_matches('\0')
            );
        }
        Command::Start(capture) => {
            let generation = inspector.trace_start(&capture.config())?;
            println!(
                "perf trace: capture {generation} started; automatic stop after {} ms",
                capture.duration_ms
            );
        }
        Command::Stop { generation } => {
            let info = require_generation(inspector, generation)?;
            stop(inspector, info.generation)?;
            println!("perf trace: capture {} stopped", info.generation);
        }
        Command::Clear { generation } => {
            let info = require_generation(inspector, generation)?;
            inspector.trace_clear(info.generation)?;
            println!("perf trace: capture {} cleared", info.generation);
        }
        Command::Dump { export, generation } => {
            let info = require_generation(inspector, generation)?;
            let output = writer(&export)?;
            stop(inspector, info.generation)?;
            let mut cursors = vec![0; info.cpu_count as usize];
            let mut capture = model::Capture::new(info, export.max_events as usize);
            collect(
                inspector,
                tasks,
                &mut capture,
                &mut cursors,
                !export.no_names,
            )?;
            if !export.no_names {
                machines.refresh(&mut capture);
            }
            write(&mut capture, &export, output)?;
        }
        Command::Record {
            capture: config,
            export,
            poll_ms,
            snapshot,
            command,
        } => {
            if !command.is_empty() && export.output == "-" {
                return Err(
                    "a workload command requires --output FILE to keep its output separate".into(),
                );
            }
            let output = writer(&export)?;
            // Optional discovery can wait for a slow VM manager. Keep that
            // wait outside the recording deadline and before workload launch.
            let vms = if export.no_names {
                Default::default()
            } else {
                machines.prepare()
            };
            let generation = inspector.trace_start(&config.config())?;
            let mut child = None;
            let result = (|| -> Result<model::Capture> {
                let info = require_generation(inspector, Some(generation))?;
                let mut cursors = vec![0; info.cpu_count as usize];
                let mut capture = model::Capture::new(info, export.max_events as usize);
                capture.vms = vms;
                if let Some(program) = command.first() {
                    child = Some(
                        std::process::Command::new(program)
                            .args(&command[1..])
                            .spawn()?,
                    );
                }
                loop {
                    // Bounded snapshots keep readers off live producer rings;
                    // export runs only after capture admission has closed.
                    if !snapshot {
                        collect(
                            inspector,
                            tasks,
                            &mut capture,
                            &mut cursors,
                            !export.no_names,
                        )?;
                    }
                    if !export.no_names {
                        machines.refresh(&mut capture);
                    }
                    if capture.limited {
                        break;
                    }
                    if let Some(child) = child.as_mut()
                        && child.try_wait()?.is_some()
                    {
                        stop(inspector, generation)?;
                    }
                    let info = require_generation(inspector, Some(generation))?;
                    if info.running == 0 {
                        // Expiry may have closed admission while a writer still
                        // owns its ring. Join before the final stable-tail read.
                        stop(inspector, generation)?;
                        collect(
                            inspector,
                            tasks,
                            &mut capture,
                            &mut cursors,
                            !export.no_names,
                        )?;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(poll_ms));
                }
                Ok(capture)
            })();
            let stopped = stop(inspector, generation);
            if (result.is_err() || stopped.is_err())
                && let Some(child) = child.as_mut()
            {
                let _ = child.kill();
                let _ = child.wait();
            }
            let mut capture = result?;
            stopped?;
            if !export.no_names {
                machines.finish(&mut capture);
            }
            if let Err(error) = write(&mut capture, &export, output) {
                if let Some(child) = child.as_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return Err(error);
            }
            // The time limit bounds capture, not the workload's execution.
            let status = child.as_mut().map(|child| child.wait()).transpose()?;
            if let Some(status) = status
                && !status.success()
            {
                return Ok(std::process::ExitCode::from(
                    status
                        .code()
                        .and_then(|n| u8::try_from(n).ok())
                        .unwrap_or(1),
                ));
            }
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}
