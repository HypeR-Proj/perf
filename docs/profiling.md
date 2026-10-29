<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# PMU sampling and flame graphs

HypeR can sample the AArch64 PMUv3 cycle counter through real overflow interrupts.
The first implementation records system-wide samples on every online CPU,
unwinds bounded **kernel** frame-pointer chains, and labels Native-user and guest
execution without reading their stacks. `perf` exports JSON-lines; the host tool
produces folded stacks and a standalone SVG. This is not Linux perf-data format.

For scheduler, syscall, IPC and I/O timelines, use the independent
[`perf trace` / Perfetto interface](tracing.md). It does not need PMU hardware.

## Build and authority

The production feature `pmu-profiling` is **off by default**, independently of
`kernel-test`. Enable **PMUv3 sampling** (`CONFIG_FEATURE_PMU_PROFILING`)
under **Kernel → Optional kernel functionality** in `make menuconfig`. For an
isolated build, an explicit override also works:

```sh
make board-build ARCH=aarch64 KERNEL_FEATURES='rtc runtime-symbols crash-console pmu-profiling'
# Raspberry Pi 5 deployment, when preparing an SD image:
make rpi5-sd ARCH=aarch64 KERNEL_FEATURES='rtc runtime-symbols crash-console pmu-profiling'
```

The product's advanced `CONFIG_KERNEL_FEATURES` override can select the same
production feature list; leave it at `"default"` to follow the kernel menu.
`perf` belongs to the ordinary system application profile and can be omitted through product component selection.
Without the feature, or without a supported firmware PMU, its queries report
`NOT_SUPPORTED`. RISC-V and x86-64 currently report unsupported as well.

The host exclusively owns the PMU. Lower-EL accesses are trapped; the virtual
CPU does not advertise a guest PMU. Firmware must describe one compatible PMU
and all interrupt routes. QEMU supplies a PPI; the Pi 5 firmware supplies a
Cortex-A76 PMU with four SPIs and CPU phandle affinity. HypeR routes each SPI to
its actual GIC CPU target, not an assumed logical-index bit. Missing or ambiguous
routes leave profiling unavailable. No physical interrupt number is guessed.

`CpuInspector` separates ordinary `INSPECT` CPU-time observations from
`SET_ATTRIBUTES` capture control and `INSPECT_DETAILS` sample access. The shell
passes profiling authority only to the trusted `/bin/perf` path; `top` retains
its ordinary read-only observation grant. A copied tool does not acquire this
authority. A controller can stop or replace a completed global capture; this is
not a private per-process tracing facility.

## Record a workload

For an ordinary command which needs filesystem and stdio capabilities:

```sh
perf record --seconds 120 --period 10000000 --output /data/storage-profile.jsonl -- /bin/storage-qual read /data/storage-qual.bin 101
```

The command runs while `perf` drains the rings. Recording ends when the command
exits or the time limit expires. At the limit, sampling stops; the command is
allowed to finish and its status is checked. Output-file or sampling failures
stop the capture and terminate/reap a command started by this invocation.
`--output` is required with a command so command stdout cannot corrupt JSON.
The child gets the ordinary std process environment, **not** the shell's
privileged tool delegation. To profile `vmm` or another privileged command,
control the capture from the shell itself:

```sh
perf start --seconds 30 --period 100000000
vmm start alpine
perf dump --output /data/start-profile.jsonl
perf status
```

`start` returns immediately and the recording survives the tool's exit.
`dump` stops the current capture and exports its retained tail. `stop` stops
without exporting; repeated dumps are non-destructive. `record` without a
command streams until its deadline (useful from another session).

There are 256 retained samples per CPU. A `start`/workload/`dump` sequence loses
older samples once this fills: at a 2.4 GHz counting rate a 100-million-cycle
period retains roughly 10.7 seconds per CPU. Use streamed `record` for long
workloads, or a larger period for a longer but coarser retained tail. Neither
a JSON file nor a flame graph hides this loss: sequence gaps and IRQ contention
drops are reported separately. An export error leaves an incomplete file which
the converter rejects rather than presenting it as a complete capture.

## Convert on the host

Copy the JSON-lines file to the host (for example from the SD data partition).
Preserve the **unstripped kernel ELF from the same image build** before rebuilding:

```sh
cp kernel/target/aarch64-unknown-none-softfloat/kernel/hyper /tmp/profile-kernel.elf
python3 -B scripts/profiling/flamegraph.py storage-profile.jsonl \
  --kernel /tmp/profile-kernel.elf --output storage-profile.svg
```

The tool locates `llvm-nm`, or accepts `--nm PATH`. It checks the embedded kernel
revision and build timestamp, resolves image-relative addresses despite KASLR,
and writes both `storage-profile.svg` and `storage-profile.folded`. The folded
file can also feed other flame-graph renderers. The SVG has function-name and
sample-count hover labels; horizontal width is aggregate sample share, **not a
timeline**. Unknown addresses and incomplete call chains remain explicitly
visible. An empty or truncated capture cannot produce a successful graph.

## Cost and limits

- Buffers occupy about 48 KiB per **configured maximum** CPU (384 KiB at the
  default maximum of eight), plus metadata, only in profiling builds. These
  bounded static buffers remain allocated between captures; they are not page
  cache. Feature-off builds contain no PMU buffers or IRQ sampling hook.
- Each CPU initializes and disables its PMU once during CPU startup. PMU
  overflow is a registered IRQ handler, selected by ordinary IRQ-domain routing.
  There is no PMU observer, route filter, or capture-state check in common
  exception entry. The generic IRQ frame view borrows the private frame without
  copying registers; only the selected PMU handler requests a snapshot. Start
  and stop publish a generation-qualified request and notify other CPUs using
  the existing kernel RPC IPI doorbell. This does not allocate a new interrupt
  or use the synchronous RPC transaction/wait path. Each CPU programs its own
  registers.
- The PMU handler runs under the IRQ registry lock, like other registered
  handlers. It does not allocate or symbolize, never waits for a buffer or
  scheduler lock, and dereferences only validated local kernel-stack frames.
  It disables the counter while collecting at most 16 frames. Buffer contention
  drops a sample instead of waiting.
- A capture lasts 1–300 seconds; a period is 1 million–1 billion **cycles**, not
  nanoseconds. All CPUs acknowledge disarming before another capture can reset
  their buffers. A kernel callout requests stop at the deadline even if the
  controller has exited or no PMU overflow occurs. Deadline delivery depends on
  the timer worker being scheduled and target CPUs servicing their control
  notifications. Stop reports `BUSY` while a CPU still owns the capture; buffers
  remain retained until it disarms. Late pending PMU interrupts are acknowledged
  even after a capture stops.
- This version samples cycles only. It has no event multiplexing, instruction or
  cache-miss event selection, per-thread attribution, user/guest symbolization,
  DWARF unwinding, or precise-event/skid correction. Native and guest execution
  appear as world buckets. IRQ masking and hardware interrupt latency bias the
  observed PC; samples are statistical evidence, not an instruction trace.
- Clock gating, idle behavior, DVFS and CPU implementation affect cycle counts.
  QEMU validates PMU delivery and export mechanics; TCG cycle numbers are not
  measurements of Pi 5 performance. Hardware results require a real Pi run.
- Exporting itself uses CPU, IPC and memory. Compare workloads with identical
  periods and check losses. The minimum period is a guardrail, not an assertion
  that maximum-rate sampling is cheap.

## Validation

`make test-pmu ARCH=aarch64` builds an opt-in kernel and a development Native
fixture. The QEMU scenario checks per-CPU overflow, bounded ring overwrite,
exclusive capture admission, IPI start/stop on every CPU, autonomous deadline
stop, cancelled deadlines crossing a new capture, restart generations, executable
path authority, and exact-ELF flame-graph generation. CI runs both GICv3 and
GICv2. Host tests cover GIC SPI routing; Loom exercises the production admission
state with stop/recycle racing an IRQ writer and with a stale deadline.

On a four-core Raspberry Pi 5, a file-copy capture collected 9,323 samples
across all four CPUs, with no ring overwrites and one IRQ contention drop.
A separate, coarser capture of repeated copies produced 65 samples with no
reported loss; exporting it and symbolizing against the exact image ELF produced
folded stacks and an SVG. Automatic stop, duplicate-start rejection and explicit
all-CPU disarming were also checked on the board. These runs validate capture
and export mechanics, not storage throughput or a representative hotspot
distribution.

The architecture follows Arm's [PMUv3 register definitions](https://developer.arm.com/documentation/ddi0601/latest/)
and the [Arm PMU firmware binding](https://www.kernel.org/doc/Documentation/devicetree/bindings/arm/pmu.yaml).
The renderer is repository-owned; it needs Python and LLVM, with no downloaded
plotting scripts or runtime web dependencies.
