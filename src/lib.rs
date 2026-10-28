// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0
//! Bounded trace collection and portable Perfetto export for the perf tool.
#[path = "trace/export/mod.rs"]
pub mod export;
#[path = "trace/model.rs"]
pub mod model;
#[path = "trace/options.rs"]
pub mod options;
#[cfg(test)]
#[path = "../tests/protobuf.rs"]
mod proto_tests;
#[cfg(test)]
#[path = "../tests/trace.rs"]
mod tests;
