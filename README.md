<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# perf

Native trace and PMU capture command.

Controls kernel profiling through delegated inspector/profiling capabilities and exports
trace records in Perfetto-compatible form. Portable formatting lives in the library
target; runtime capture belongs to the executable.

See the [implementation entry](src/lib.rs) and [component declaration](component.json)
for the public interfaces, build dependencies and delivered artifacts.

## Usage

In a Native session with the required capabilities:

```sh
perf --help
```

## Boundaries

Capture support depends on kernel configuration and hardware. Stopping a capture retains
its buffer for dump/repeated reads; release stopped static-event buffers with `perf
trace clear` after saving the trace.

## Build and test

Use this checkout inside the [repo-managed workspace](https://github.com/HypeR-Proj/HypeR).
From this repository directory:

```sh
make
make test
```

An absolute `OUT=/path/to/out` selects a shared output directory. The module
entry delegates to build/system, which prepares the installed SDK and declared
dependencies. Native binaries require HypeR; host module checks cover the tests
selected by component.json. A standalone clone does not contain the workspace’s
other source repositories or Native SDK.
