// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded PMU capture control and portable JSON-lines export.
#[cfg(target_os = "hyper")]
extern crate hyper_clap_shared as _;
use clap::{Parser, Subcommand};
use hyper_os::{Error, Status, inspect::CpuInspector};
use std::{
    io::{self, Write},
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "perf",
    about = "PMUv3 cycle sampling (requires a pmu-profiling kernel)"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Start recording; the kernel stops automatically at the time limit.
    Start {
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=300))]
        seconds: u64,
        #[arg(long, default_value_t = 10_000_000, value_parser = clap::value_parser!(u64).range(1_000_000..=1_000_000_000))]
        period: u64,
    },
    /// Stop the current capture without discarding its samples.
    Stop,
    /// Display capture state and the exact kernel revision/build timestamp.
    Status,
    /// Stop and export retained samples. Redirect stdout or use --output.
    Dump {
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Stream samples, optionally while running an ordinary command after --.
    Record {
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=300))]
        seconds: u64,
        #[arg(long, default_value_t = 10_000_000, value_parser = clap::value_parser!(u64).range(1_000_000..=1_000_000_000))]
        period: u64,
        #[arg(short, long)]
        output: Option<String>,
        #[arg(last = true)]
        command: Vec<String>,
    },
}
fn stop(inspector: &CpuInspector, generation: u64) -> Result<(), Box<dyn std::error::Error>> {
    if generation == 0 {
        return Err("no capture has been started".into());
    }
    for _ in 0..100 {
        match inspector.profile_stop(generation) {
            Ok(()) => return Ok(()),
            Err(Error::Status(Status::BUSY)) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(error.into()),
        }
    }
    Err("PMU stop acknowledgement timed out; capture is disabled, retry stop".into())
}
fn export(
    inspector: &CpuInspector,
    generation: u64,
    stream: bool,
    output: Option<String>,
    mut child: Option<&mut std::process::Child>,
) -> Result<(), Box<dyn std::error::Error>> {
    let info = inspector.profile_info()?;
    if info.generation != generation {
        return Err("capture replaced before exporting".into());
    }
    if !stream {
        stop(inspector, info.generation)?;
    }
    let writer: Box<dyn Write> = match output {
        Some(path) => Box::new(std::fs::File::create(path)?),
        None => Box::new(io::stdout()),
    };
    let mut writer = io::BufWriter::new(writer);
    let revision = std::str::from_utf8(&info.revision)?.trim_end_matches('\0');
    writeln!(
        writer,
        "{}",
        serde_json::json!({"format":"hyper-pmu-v1", "kernel":revision, "generation":info.generation, "period":info.period, "cpus":info.cpu_count, "capacity_per_cpu":info.capacity, "event":"cpu-cycles", "address":"kernel-image-offset"})
    )?;
    let mut cursors = vec![0; info.cpu_count as usize];
    let mut lost = 0u64;
    let mut samples = 0u64;
    let mut finished = !stream;
    loop {
        for (cpu, cursor) in cursors.iter_mut().enumerate() {
            // A bounded pass gives other CPUs and the deadline a chance even
            // when this CPU produces faster than the exporter can consume.
            for _ in 0..info.capacity {
                let sample = match inspector.profile_sample(info.generation, cpu as u64, *cursor) {
                    Ok(sample) => sample,
                    Err(Error::Status(Status::WOULD_BLOCK)) => break,
                    Err(error) => return Err(error.into()),
                };
                lost += sample.sequence - *cursor;
                *cursor = sample.sequence + 1;
                let frames: Vec<String> = sample
                    .frames
                    .chunks_exact(8)
                    .take(sample.depth as usize)
                    .map(|b| {
                        let mut word = [0; 8];
                        word.copy_from_slice(b);
                        format!("{:x}", u64::from_le_bytes(word))
                    })
                    .collect();
                writeln!(
                    writer,
                    "{}",
                    serde_json::json!({"cpu":cpu, "sequence":sample.sequence, "time_ns":sample.timestamp_ns, "world":sample.world, "incomplete":sample.flags != 0, "dropped":sample.dropped, "frames":frames})
                )?;
                samples += 1;
            }
        }
        writer.flush()?;
        if finished {
            break;
        }
        if let Some(child) = child.as_mut()
            && child.try_wait()?.is_some()
        {
            stop(inspector, info.generation)?;
        }
        let current = inspector.profile_info()?;
        if current.generation != info.generation {
            return Err("capture replaced while exporting".into());
        }
        if current.running_generation == 0 && current.active_cpus == 0 {
            finished = true;
            continue;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let final_info = inspector.profile_info()?;
    if final_info.generation != info.generation {
        return Err("capture replaced while exporting".into());
    }
    writeln!(
        writer,
        "{}",
        serde_json::json!({"end":true,"samples":samples,"overwritten":lost,"irq_dropped":final_info.dropped})
    )?;
    writer.flush()?;
    eprintln!("perf: {samples} samples exported; {lost} overwritten before reading");
    Ok(())
}
fn run() -> Result<std::process::ExitCode, Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut startup = hyper_rt::process::startup()?;
    hyper_os::require_core_abi()?;
    let inspector = CpuInspector::from_handle(startup.take(hyper_os::startup::CPU_INSPECTOR)?);
    match args.command {
        Command::Start { seconds, period } => {
            let id = inspector.profile_start(period, seconds)?;
            println!("perf: capture {id} started, period {period} cycles, limit {seconds}s");
        }
        Command::Stop => {
            stop(&inspector, inspector.profile_info()?.generation)?;
            println!("perf: stopped");
        }
        Command::Status => {
            let info = inspector.profile_info()?;
            println!(
                "generation={} running={} armed={:#x} period={} cycles CPUs={} buffer={}/CPU\nkernel={}",
                info.generation,
                info.running_generation != 0,
                info.armed_mask,
                info.period,
                info.cpu_count,
                info.capacity,
                std::str::from_utf8(&info.revision)?.trim_end_matches('\0')
            );
        }
        Command::Dump { output } => {
            let generation = inspector.profile_info()?.generation;
            export(&inspector, generation, false, output, None)?;
        }
        Command::Record {
            seconds,
            period,
            output,
            command,
        } => {
            if !command.is_empty() && output.is_none() {
                return Err(
                    "recording a command requires --output to separate samples from command output"
                        .into(),
                );
            }
            let generation = inspector.profile_start(period, seconds)?;
            let mut child = if let Some(program) = command.first() {
                match std::process::Command::new(program)
                    .args(&command[1..])
                    .spawn()
                {
                    Ok(child) => Some(child),
                    Err(error) => {
                        let _ = stop(&inspector, generation);
                        return Err(error.into());
                    }
                }
            } else {
                None
            };
            let result = export(&inspector, generation, true, output, child.as_mut());
            let stopped = stop(&inspector, generation);
            if (result.is_err() || stopped.is_err())
                && let Some(child) = child.as_mut()
            {
                let _ = child.kill();
                let _ = child.wait();
            }
            result?;
            stopped?;
            if let Some(child) = child.as_mut() {
                let status = child.wait()?;
                if !status.success() {
                    eprintln!("perf: profiled command failed: {status}");
                    return Ok(std::process::ExitCode::from(
                        status
                            .code()
                            .and_then(|code| u8::try_from(code).ok())
                            .unwrap_or(1),
                    ));
                }
            }
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            match error.downcast_ref::<Error>() {
                Some(Error::Status(Status::NOT_SUPPORTED)) => eprintln!(
                    "perf: PMUv3 profiling is not supported by this kernel/platform; check pmu-profiling and firmware PMU routes"
                ),
                Some(Error::Status(Status::BUSY)) => eprintln!(
                    "perf: another capture is active or stopping; use perf status, stop or dump"
                ),
                Some(Error::Status(Status::ACCESS_DENIED)) => {
                    eprintln!("perf: CPU profiling authority was not granted")
                }
                _ => eprintln!("perf: {error}"),
            }
            std::process::ExitCode::FAILURE
        }
    }
}
