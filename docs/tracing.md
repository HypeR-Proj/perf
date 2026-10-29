<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# Kernel timelines with Perfetto

`perf trace` records static kernel events into bounded CPU-local rings and exports
native Perfetto protobuf. It uses the public [GenericKernel and TrackEvent protocols](https://github.com/google/perfetto/tree/v58.2/protos/perfetto/trace), including real scheduler analysis tables.
Open the exported file at <https://ui.perfetto.dev>. PMU samples/flame graphs remain
available through the independent [PMU commands](profiling.md).

The default view has CPU scheduling tracks and process-grouped Threads, with
Running, Runnable and Sleeping states. Thread activity tracks contain IPC/RPC
flows, syscall intervals and switch-out reasons. `perf` translates existing
observations in userspace; no additional kernel event is needed. Add
`--raw-events` to include raw instant observations. `--format chrome` retains
Chrome Trace JSON for diagnostics; `--format jsonl` retains raw records.

## Build and capture

Enable `kernel-trace` in the kernel feature menu and deploy `perf` in the system
application menu. The feature is off by default. It requires neither
`pmu-profiling` nor test features. For a QEMU development fixture:

```sh
make test-trace ARCH=aarch64
```

On HypeR, from the authorized formal shell:

```sh
perf trace record --duration-ms 5000 --categories scheduler,syscall,ipc,io \
  --cpus all --buffer-records 16384 --poll-ms 10 \
  --max-events 262144 --output /tmp/trace.pftrace && perf trace clear

perf trace record --duration-ms 10000 --categories scheduler,io \
  --output /tmp/storage.pftrace -- /bin/storage-qual read /data/storage-qual.bin 101 && perf trace clear
```

The workload must be deployed separately. Recording stops when the command exits,
the recording deadline arrives, or the collector limit is reached. A running
command continues after the recording deadline; `perf` waits for it and propagates
its exit status. Children receive ordinary process capabilities, not the shell's
privileged-tool delegation; profile `vmm` with separate `start` and `dump` calls.
The deadline bounds tracing, not workload execution. Any
collector error closes recording and cleans up its own child.

For a retained-tail capture without a resident collector:

```sh
perf trace start --duration-ms 10000 --categories scheduler,vm --cpus 0-3
vmm list
perf trace status
perf trace dump --output /data/trace.pftrace && perf trace clear
```

`dump` stops recording before reading. `stop` is also available separately.
Both accept `--generation N` to reject an unexpected replacement capture.
`clear` also accepts this guard and releases a stopped capture's buffers; it
rejects a running capture and needs no new memory allocation to release buffers.
Omit `clear` when another dump is needed.
Starting again discards the previous capture. Reading is non-destructive, so
another dump is possible until the next start or clear. The kernel stops at the deadline
even if the collecting process exits unexpectedly.

`/tmp` is volatile. For bulk export, copy the completed file to a writable
filesystem and retrieve it through its backend. The QEMU harness below unmounts
its data volume and extracts the file from the stopped disk image. `-o -` emits
the selected format on stdout, with diagnostics on stderr; it cannot be combined with a workload
command because child output would corrupt that stream.

## Capture controls

| Option | Default | Meaning |
| --- | --- | --- |
| `--duration-ms` | 10000 | Kernel-enforced duration, 1–300000 ms |
| `--categories` | `scheduler,vm,ipc,io` | Comma-separated categories below, or `all` |
| `--cpus` | `all` | Online indices/ranges, e.g. `0,2-3`; offline CPUs fail |
| `--buffer-records` | 16384 | Records allocated per selected CPU at start, 64–16384 |
| `--poll-ms` | 10 | `record` polling interval, 1–1000 ms; snapshot mode polls child/deadline state only |
| `--snapshot` | off | `record` reads only after stopping; suitable for short workloads that fit the per-CPU rings |
| `--max-events` | 262144 | Userspace event bound, 1–4000000 |
| `--output`, `-o` | `/tmp/trace.pftrace` | File or `-` for stdout |
| `--format` | `perfetto` | Native `perfetto` protobuf, diagnostic `chrome` JSON, or raw `jsonl` |
| `--raw-events` | off | Include raw instant tracepoints in the Perfetto view; does not change kernel capture or JSONL |
| `--no-names` | off | Skip live name/owner lookups; retain identities recorded in events |
| `--fail-on-loss` | off | Return failure if overwrite, incomplete stream or collector limit occurred; retain output |
| `--overwrite` | off | Explicitly allow replacing an existing file |

Options belong to `start`, `record` or `dump` as appropriate; use
`perf trace COMMAND --help` for the exact syntax. `status` shows the generation,
selected CPUs, kernel identity, deadline and buffer bounds. Captures are global,
not a per-process private resource. Start/stop/clear require CpuInspector
SET_ATTRIBUTES; reads require INSPECT_DETAILS. Copying `perf` outside the trusted
`/bin/perf` launch path does not grant authority. The authorized shell also
delegates read-only TaskInspector INSPECT for live process/thread grouping;
standalone callers may omit it. Recorded `syscall_enter` events preserve process
ownership even if the Thread has already been reaped before a snapshot or dump.
When the process name is unavailable, it is shown as `process[KOID]`. Without
either live ownership metadata or a recorded process identity, the Thread remains
in the `threads` fallback group; its parent is not guessed. Include `syscall`
when capturing short-lived Native workloads whose live metadata may disappear.

## Events and interpretation

| Category | Recorded observations |
| --- | --- |
| `scheduler` | Committed switches and their causes, reschedule requests/sources, wait registration/completion and outcomes, wakeups and migration publication |
| `syscall` | Ordinary Native dispatch entry/return, operation and object kind, signed status |
| `ipc` | Byte/capability commit identities, delivery lengths, signal wait/notification, explicit RPC annotations and syscall-level outcomes |
| `io` | Logical block admission/completion/release, submitting Thread, exact batch-to-command links, transport slots and command completion/abort |
| `memory` | Native page-fault handling entry/return and resolution |
| `irq` | Physical IRQ service entry/return with interrupt ID, plus architecture-private kernel RPC delivery |
| `vm` | vCPU run entry/return to its kernel runner with exit reason |

Events are scalar observations; they contain no kernel addresses, user payloads
or raw handle values. Successful receives report delivered bytes; failed receives report zero.

One `sched_switch` record contains both Thread identities, packed state/class,
and packed cause/entry point. The exporter expands the byte fields into named
arguments: `state` holds `prev_state` in bits 0–7 and `next_class` in bits 8–15;
`cause` holds `reason` in bits 0–7 and `point` in bits 8–15. There is no separate
cause record to consume ring capacity or lose independently. `reason` is
1 yield, 2 policy preemption, 3 blocking, 4 exit, 5 migration, or 6 idle
dispatch. `point` is 1 explicit operation, 2 cooperative safe point, or 3 IRQ
tail. `perf` adds readable names and `preempted` to the observation and to the
Thread activity track as **Switch out** (and the CPU slice in Chrome JSON). It is true for policy preemption and migration at
a safe point or IRQ tail; migration committed by an explicit yield is voluntary.
A safe-point switch can still preempt a runnable Thread; the path is distinct
from its reason.
Missing metadata and unknown values remain unknown rather than inferring
preemption from a Ready state. A retained switch carries its own cause even
when earlier records are lost; execution durations still require intact bounds.

`sched_request` records the target CPU, publishing Thread, whether an explicit
notification was elected, and the request source: 1 wakeup, 2 scheduler deadline,
3 stop, 4 policy change, or 5 migration. Only transitions from not-pending to
pending are recorded. Repeated requests that coalesce into an existing pending
decision and internal load-balancing prompts do not consume records. Actual
migration and switch events remain visible. These are request observations,
not an exhaustive list of every contributor to a pending decision, and do not
necessarily switch Threads. Enable `scheduler,irq` together to obtain
`irq_context` on requests enclosed by a complete, matching local IRQ interval.
It identifies a physical interrupt number or a private kernel-RPC prompt; gaps,
unmatched boundaries, other-CPU IRQs and absent IRQ recording leave it null.
An IRQ context describes where a request was published, not proof that this
IRQ uniquely caused a later switch. Timer callbacks run in normal worker
context, so their deadline requests need not themselves have an enclosing IRQ.
Opposite channel endpoints have different KOIDs. Core `ipc_enqueue`/`ipc_dequeue`
and `cap_enqueue`/`cap_dequeue` use a shared, boot-local connection identity and
separate destination queues, plus the exact committed message/registration ID.
They also cover kernel-originated ByteChannel messages, including remote VFS
requests. These trace IDs are observations, never handles or lookup authority.
Legacy `byte_send` / `byte_receive` outcomes skip `WOULD_BLOCK` polling;
select `syscall` to inspect those attempts. Committed enqueue/dequeue records,
non-polling failures and buffer-size probes remain available under `ipc`.
Immediate fast-path syscalls, trace-control methods and RPC annotation methods
are excluded from syscall tracing to avoid collection feedback. Other collector activity is
visible and can perturb the workload.
`syscall_exit` marks completion of ordinary dispatch: `returned=1` carries the
ABI status, including a successful yield; `returned=0` carries the requested
thread/process exit code. Subsequent yield scheduling or process teardown is
outside that dispatch interval.

Native scheduling records populate Perfetto's `sched` and `thread_state` tables.
CPU tracks and per-Thread states share one identity, including migration. Threads
are grouped under observed process owners; idle Threads are identified separately.
vCPUs are grouped under `vm-{name}` using VM-owner observations of their actual
scheduler ThreadIds. The shell delegates a dedicated read-only VM observation
channel; it does not give perf VM lifecycle or affinity control. Initial VM
discovery finishes before `record` starts its deadline, so a slow observer cannot
consume the workload's capture window before launch. New vCPU threads
can trigger a bounded refresh during collection. Retired observations remain in
the capture; a vCPU without ownership metadata stays unassigned rather than being
guessed from a thread name or placed under kernel.
Native Perfetto TIDs use the full observed Thread KOID, matching `ps --threads`,
while track UUIDs remain export-local. Missing KOIDs use explicitly labelled
export-local TIDs that exclude all observed KOIDs. Synthetic PIDs avoid every TID
so Perfetto's implicit main threads cannot collide. Real process names include
their full KOID, for example `fs-backend[0x0000000100000064]`.
Capture metadata and Thread descriptors
retain the separate scheduler ThreadId and process owner when available. KOIDs
with bit 63 set retain every bit but appear signed in Perfetto's int64 TID column;
metadata also preserves their hexadecimal form. An optional read-only query resolves
ThreadId to KOID; TaskInspector supplies the live owner. Neither ID grants
scheduling authority. `sched_reap` also retains the KOID after live lookup becomes
unavailable. Chrome JSON keeps export-local track IDs. No Linux scheduler or signal
ABI is implied:
HypeR Blocked maps to generic Sleeping, Ready/Migrating to Runnable, and Terminated
to Dead (shown as Zombie). A subsequent `sched_reap` records completed execution
resource retirement and maps to Destroyed, closing the Zombie interval and setting
the thread's end timestamp. If reaping falls outside the capture or its event is
lost/filtered, that endpoint remains unknown. Exit and retirement at the same
timestamp collapse to Destroyed without a zero-length Zombie interval.
Actual priority values are not currently captured; `prio` is omitted instead of
substituting a scheduling class. GenericKernel does not encode the
Linux `R+` distinction; **Switch out** retains the actual `preempted` flag and
HypeR scheduling reason/entry point.

Thread activity intervals use reusable child tracks so interleaved RPCs retain
their own start and end times without being interpreted as nested calls.
They show elapsed syscall/fault/vCPU time, including sleep
and descheduling; they are not CPU usage. vCPU boundaries are returns to the runner,
not every hardware exit handled by the HAL fast path. IRQ spans cover ordinary
routed service and private kernel-RPC delivery, excluding scheduling postludes.

Block transport tracks use separate slots for concurrent commands. Logical batch
tracks use the queue's ticket slot and distinguish admission-to-completion from
**Completion retained**, the time until reap or discard releases queue credits.
**Dispatch delay** includes software queueing and preparation before the first
command is published. It excludes quota waits before successful admission.
**Block admission**, **Block dispatch** and **Block completion** arrows connect
the submitting Thread, logical batches and physical commands. Merging and
splitting are explicit many-to-many links, with contributed byte counts.
Read, write, flush and control commands are distinct; an aborted command closes
its observed lifetime, without claiming successful DMA quiescence. Counters count
only commands with both boundaries captured, so they are not whole-device queue
depth measurements. These observations stop at Native block transport and do not
measure Linux I/O VM or physical controller internals. Missing slot metadata,
unmatched/duplicate identities, loss or a partial CPU selection suppress derived
associations.

`--raw-events` adds source locations and scalar fields.
JSONL and native protobuf retain nanoseconds; Chrome JSON uses microseconds
relative to capture start with fractional precision. Native metadata is stored in
the root HypeR track description, without additional capture-quality notices.

Example analysis in Perfetto's SQL view:

```sql
SELECT s.ts, s.dur, s.cpu, t.name AS thread, p.name AS process, s.end_state
FROM sched s
JOIN thread t USING (utid)
LEFT JOIN process p USING (upid)
ORDER BY s.ts;

SELECT t.name, s.state, SUM(s.dur) AS duration_ns
FROM thread_state s
JOIN thread t USING (utid)
WHERE s.dur >= 0
GROUP BY t.utid, s.state;
```

## IPC and critical chains

Select **all CPUs**, and record at least `ipc,scheduler`. Expand thread lanes in
Perfetto and select a **Message delivery** or **RPC dispatch** slice to follow its
flow arrow. The destination includes `latency_ns`; inspect raw events for queue,
message, connection and request IDs. Equal-timestamp message boundaries match by
queue and message identity, even if CPU sorting visits the receive first; their
latency is zero. **Wakeup → Scheduled** connects an observed waker to the matching Thread
dispatch after a bounded Sleeping interval. Chrome JSON also retains the
**Wake to execution** wait-chain view for complete captures. `Blocked (notified/timeout/cancelled)`
measures wait-registration completion; **Runnable** shows subsequent scheduling
delay. These are elapsed observations, not a computed whole-system critical path.
A signal wake identifies the winning source; it does not assert which of several
queued messages caused that level signal. `waker` names the executing thread
context, which may have been interrupted by an IRQ; it is not proof that its
application initiated the notification. Persistent WaitSet notifications retain
scheduler wait timing but do not identify the original signal source.

`hyper_os::channel::RpcTracer` provides optional client/server begin/end annotations.
Construct one tracer per READ|WRITE endpoint. Both sides use the same nonzero
protocol request ID, unique among concurrent calls on their connection. The
exporter suppresses RPC spans and flows for IDs with overlapping annotations
throughout the capture, including edges preceding discovery of the overlap.
Raw annotations remain visible. Pass an
explicit `RpcContext` to a nested call to create **RPC parent** flows; no ambient
thread state or message-payload parsing infers parentage. **RPC round trip** and
**RPC handler** include completion detail, including failures. The common formal
filesystem worker and kernel remote VFS client use this interface. The bootstrap
filesystem implementation stays independent; its core messages are still traced.
No I/O VM Linux-internal spans or end-to-end physical I/O attribution are implied.

Begins/ends outside a capture, unreceived messages, aborted message preparations and
aborted receive claims never manufacture complete delivery. A stopped/replaced
capture invalidates saved RPC contexts. Duplicate active request IDs suppress
further derived RPC intervals for that ID. User annotations describe their own
protocol; they grant no ability to enable or read global tracing. No allocation,
registry lookup, payload inspection or global per-message ID counter is added to
the producer. Compiling out `kernel-trace` removes its object metadata, actor
mirror and recording paths. Formal filesystem workers then probe support once
per connection, with no per-request annotation invocations. With tracing compiled
in, explicit annotations cost an invocation per boundary even while capture is off.

The bounded development fixture exercises duplex, nested RPC and actual waits:

```sh
perf trace record --snapshot --duration-ms 30000 --categories ipc,scheduler --no-names \
  --poll-ms 1 --fail-on-loss -o /tmp/ipc.pftrace -- /bin/lifecycle-stress trace
```

Host tests can export an import fixture, and an installed upstream Perfetto
`trace_processor` can validate its actual slice/flow tables:

```sh
HYPER_TRACE_PROTO_RPC_FIXTURE=/tmp/ipc-fixture.pftrace \
  HYPER_TRACE_PROTO_SCHED_FIXTURE=/tmp/sched-fixture.pftrace \
  make app-test APP_TEST_PACKAGE=hyper-perf
python3 -B tests/profiling/verify-perfetto.py /tmp/ipc-fixture.pftrace \
  --processor /path/to/trace_processor
python3 -B tests/profiling/verify-perfetto.py /tmp/sched-fixture.pftrace \
  --processor /path/to/trace_processor --scheduling
```

The same import check accepts `target/app/aarch64/trace-4.ipc.json` produced by
`make test-trace`; it checks every exported arrow and rejects importer errors.
For an ordinary timeline without causal flows, pass `--scheduling` to validate
the native execution intervals against Perfetto's actual `sched` and
`thread_state` tables. Both modes check flow counts and reject importer errors.
Both QEMU trace targets boot a disposable disk-backed system with a minimal I/O
VM, a system EROFS volume and a 64 MiB FAT data volume; no Alpine disk is needed.
`test-trace` covers lifecycle, loss, explicit nested RPC and a remote FAT mutation.
Its complete causal captures use one `record --snapshot` invocation: no live ring
reader contends with producers, and no start/dump command roundtrip fills the rings
with terminal traffic. Buffer capacity and loss checks still apply; snapshot mode
retains only what fits in the compiled per-CPU rings.
The FAT scenario warms executable and source pages before capturing only `ipc,io`
events for the mutation, leaving room even in the one-CPU ring. Scheduler coverage
is checked in the separate bounded IPC workload.
After successful export, both harnesses issue `perf trace clear`; failed exports
retain the capture for diagnosis. The repeated-dump probe clears only after both
files have been produced.
`make test-trace-remote ARCH=aarch64` runs only the remote filesystem scenario.

Trace bodies are never printed through the console. Capture/export writes `/tmp`
first; after recording stops, the harness copies the files through the filesystem
backend to `/data/trace-artifact-*`, then unmounts `/data` to fence data and metadata.
After the owned QEMU exits, the host validates both GPT copies and uses `mcopy`
with the selected FAT partition offset to extract the original bytes. No host
mount or privileged loop device is needed. The serial connection carries bounded
command results and capture summaries only.

The complete raw files are retained under
`target/app/aarch64/trace-<cpus>.artifacts/`; the main timeline and IPC timeline
also keep their existing `trace-<cpus>.json` and `trace-<cpus>.ipc.json` paths.
CI uploads the raw files even when host-side content validation fails. A failed
guest command retains its serial command report and the disposable disk under
`target/board-tests/trace.*/` for local diagnosis.

## Bounds and loss

Payload storage is allocated at `start`, only for selected CPUs and the requested
record count. The default is 16384 × 56 bytes (896 KiB) per selected CPU, or 3.5 MiB
for four CPUs, plus small control headers. No payload buffers exist before the
first capture or after `clear`. `stop` retains them for repeatable export.
Replacement prepares new buffers before releasing the old capture, so allocation
failure preserves the old data and temporary usage includes both captures.
Without `kernel-trace`, storage and instrumentation compile out. Runtime-disabled points only
check an atomic category flag; enabled points mask local IRQs briefly and use
try-only ring access, without allocation or cross-CPU producer locking.
Feature-enabled kernels also publish one CPU-local actor scalar per context switch
and retain trace-only object/wait identities while recording is stopped.

The collector stores approximately 64 bytes per event plus interned names/sites.
It stops before exporting the file, avoiding export I/O inside the measured
interval. Native export builds bounded packet storage proportional to the
collected records, sorts derived endpoints, and serializes them in timestamp
order. This storage is released after export. Large JSON output can be
substantially larger than the compact collection. Duration, ring capacity and
event count are independent bounds.

Overwrites are counted per CPU. Contention/recursion/clock failure is a sticky
`incomplete` flag, not an exact dropped-event count. Loss is retained in capture
metadata; `--raw-events` also includes `TRACE LOSS`/`TRACE GAP` markers. Missing events invalidate pending spans;
capture loss suppresses repeatable RPC annotations and elapsed syscall/wait
intervals, since missing boundaries can hide another invocation. Committed IPC
message sequences and device tags are non-reused within their unique queue/domain:
matching observed endpoints remain valid despite unrelated source loss. Explicit
sequence gaps still invalidate pending pairs conservatively.
Matching switch-in/out identities still form an observed window, shown as
native CPU execution and Thread states (or **Running**/**Idle** in Chrome JSON). Thread names stay unchanged and no
additional capture-quality notices are displayed. Unlocated loss can hide
intermediate switches, so a window's width is not necessarily precise CPU time.
Explicit sequence gaps, missing endpoints and mismatched Thread identities leave
holes. Overlapping observations of one Thread on multiple CPUs are omitted from
both execution views instead of inventing a nesting order.
Selecting only part of the online CPU set also
suppresses repeatable RPC and elapsed Thread intervals: a migrated thread may have unmatched boundaries
on an unselected CPU. Local CPU and IRQ intervals remain available. Neither unmatched starts nor truncated tails are
invented into complete intervals. Increase capacity, reduce categories or drain
more frequently to reduce overwrite; collecting itself also has overhead.

Native scheduling uses the OS-independent `sched`/`thread_state` tables;
activities and arrows use `slice`/`args`/`flow`. HypeR does not emit synthetic Linux
ftrace records. No live Perfetto daemon or tracing service is installed.
