// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0
use clap::{Args, Subcommand, ValueEnum};
use hyper_os::inspect::trace::{Category, TraceConfig};

#[derive(Subcommand)]
pub enum Command {
    /// Start a bounded snapshot capture; no collector needs to remain alive.
    Start(Capture),
    /// Close admission and wait for in-flight writers.
    Stop {
        /// Reject control if a different capture has replaced this generation.
        #[arg(long)]
        generation: Option<u64>,
    },
    /// Show capture state, kernel identity, selected CPUs and buffer bounds.
    Status,
    /// Stop and export the retained tail of a capture.
    Dump {
        #[command(flatten)]
        export: Export,
        /// Reject control if a different capture has replaced this generation.
        #[arg(long)]
        generation: Option<u64>,
    },
    /// Drain CPU rings into bounded userspace storage, optionally running a command.
    Record {
        #[command(flatten)]
        capture: Capture,
        #[command(flatten)]
        export: Export,
        /// Collector sleep interval. Smaller values reduce overwrite at higher overhead.
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=1000))]
        poll_ms: u64,
        #[arg(last = true)]
        command: Vec<String>,
    },
}
#[derive(Args)]
pub struct Capture {
    /// Wall-clock recording limit, enforced by the kernel even if perf exits.
    #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u64).range(1..=300_000))]
    pub duration_ms: u64,
    /// Comma-separated scheduler,ipc,syscall,io,memory,irq,vm or all.
    #[arg(long, default_value = "scheduler,vm,ipc,io", value_parser = categories)]
    pub categories: u64,
    /// Online CPU indices/ranges (for example 0,2-3); all selects every online CPU.
    #[arg(long, default_value = "all", value_parser = cpus)]
    pub cpus: CpuSelection,
    /// Retained records per CPU; does not change the compiled maximum allocation.
    #[arg(long, default_value_t = 4096, value_parser = clap::value_parser!(u64).range(64..=4096))]
    pub buffer_records: u64,
}
#[derive(Clone, Debug)]
pub struct CpuSelection(pub [u64; 4]);
impl Capture {
    pub fn config(&self) -> TraceConfig {
        let mut cpu_mask = [0; 32];
        for (dst, word) in cpu_mask.chunks_exact_mut(8).zip(self.cpus.0) {
            dst.copy_from_slice(&word.to_le_bytes());
        }
        TraceConfig {
            categories: self.categories,
            duration_ms: self.duration_ms,
            capacity: self.buffer_records,
            cpu_mask,
        }
    }
}
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Format {
    Perfetto,
    Jsonl,
}
#[derive(Args)]
pub struct Export {
    /// Output file. '-' writes stdout; a workload command requires a real file.
    #[arg(short, long, default_value = "/tmp/trace.json")]
    pub output: String,
    /// Perfetto accepts the generated Chrome Trace JSON directly.
    #[arg(long, value_enum, default_value = "perfetto")]
    pub format: Format,
    /// Bound collector storage (roughly 64 bytes/event, plus site/name metadata).
    #[arg(long, default_value_t = 262_144, value_parser = clap::value_parser!(u64).range(1..=4_000_000))]
    pub max_events: u64,
    /// Skip best-effort live thread-name lookups.
    #[arg(long)]
    pub no_names: bool,
    /// Treat any overwrite or incomplete CPU stream as a command failure.
    #[arg(long)]
    pub fail_on_loss: bool,
    /// Allow replacement of an existing output file.
    #[arg(long)]
    pub overwrite: bool,
}
pub fn categories(value: &str) -> Result<u64, String> {
    if value == "all" {
        return Ok(Category::ALL);
    }
    let mut bits = 0;
    for name in value.split(',') {
        bits |= match name {
            "scheduler" => 1,
            "ipc" => 2,
            "syscall" => 4,
            "io" => 8,
            "memory" => 16,
            "irq" => 32,
            "vm" => 64,
            _ => return Err(format!("unknown trace category {name:?}")),
        };
    }
    Ok(bits)
}
pub fn cpus(value: &str) -> Result<CpuSelection, String> {
    let mut words = [0; 4];
    if value == "all" {
        return Ok(CpuSelection(words));
    }
    for range in value.split(',') {
        let (start, end) = match range.split_once('-') {
            Some(pair) => pair,
            None => (range, range),
        };
        let parse = |n: &str| {
            n.parse::<usize>()
                .ok()
                .filter(|n| *n < 256)
                .ok_or_else(|| "CPU indices must be 0..255".to_string())
        };
        let (start, end) = (parse(start)?, parse(end)?);
        if start > end {
            return Err("CPU range is reversed".into());
        }
        for cpu in start..=end {
            words[cpu / 64] |= 1 << (cpu % 64);
        }
    }
    Ok(CpuSelection(words))
}
