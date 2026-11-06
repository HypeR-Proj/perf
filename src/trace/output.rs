// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bound export memory while amortizing synchronous filesystem writes.

use std::io::{self, BufWriter, Write};

const BUFFER_BYTES: usize = 128 * 1024;
const REPORT_BYTES: u64 = 4 * 1024 * 1024;

struct Destination<W, P> {
    writer: W,
    progress: P,
    accepted: u64,
    reported: u64,
}

impl<W: Write, P: FnMut(u64)> Write for Destination<W, P> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.writer.write(bytes)?;
        self.accepted += count as u64;
        if self.accepted - self.reported >= REPORT_BYTES {
            self.reported = self.accepted;
            (self.progress)(self.accepted);
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Coalesce small encoded packets without changing the exported bytes. Progress
/// counts accepted writes, not durable media. On failure discard buffered data:
/// `BufWriter`'s destructor must not retry I/O after reporting an export error.
pub fn buffered(
    writer: impl Write,
    serialize: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    progress: impl FnMut(u64),
) -> io::Result<u64> {
    let mut destination = Destination {
        writer,
        progress,
        accepted: 0,
        reported: 0,
    };
    let mut output = BufWriter::with_capacity(BUFFER_BYTES, &mut destination);
    let result = serialize(&mut output).and_then(|()| output.flush());
    let _ = output.into_parts();
    result?;
    Ok(destination.accepted)
}
