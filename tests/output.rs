// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::output;
use std::io::{self, Write};

#[derive(Default)]
struct Sink {
    bytes: Vec<u8>,
    writes: usize,
    flushes: usize,
    partial_error: bool,
    flush_error: bool,
}
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        if self.partial_error && self.writes == 2 {
            return Err(io::Error::other("injected write failure"));
        }
        let count = if self.partial_error && self.writes == 1 {
            bytes.len().min(17)
        } else {
            bytes.len()
        };
        self.bytes.extend_from_slice(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        if self.flush_error {
            Err(io::Error::other("injected flush failure"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn packet_sized_writes_are_batched_without_changing_bytes() -> io::Result<()> {
    let input: Vec<_> = (0..40 * 1024 * 1024 + 97).map(|n| n as u8).collect();
    let mut sink = Sink::default();
    let mut reports = Vec::new();
    let size = output::buffered(
        &mut sink,
        |writer| {
            for packet in input.chunks(1021) {
                writer.write_all(packet)?;
            }
            Ok(())
        },
        |bytes| reports.push(bytes),
    )?;
    assert_eq!(sink.bytes, input);
    assert_eq!(size, input.len() as u64);
    // An 8 KiB writer needs over 5000 writes for the same packet stream.
    assert!(sink.writes < 350, "writes={}", sink.writes);
    assert_eq!(sink.flushes, 1);
    assert!(!reports.is_empty());
    assert!(reports.windows(2).all(|p| p[1] - p[0] >= 4 * 1024 * 1024));
    assert!(reports.iter().all(|n| *n <= size));
    Ok(())
}

#[test]
fn failed_exports_do_not_retry_writes_in_drop() {
    let mut sink = Sink {
        partial_error: true,
        ..Sink::default()
    };
    let result = output::buffered(
        &mut sink,
        |writer| {
            for _ in 0..200 {
                writer.write_all(&[42; 1021])?;
            }
            Ok(())
        },
        |_| {},
    );
    assert!(result.is_err());
    assert_eq!(sink.writes, 2);
    assert_eq!(sink.bytes, vec![42; 17]);
    assert_eq!(sink.flushes, 0);

    let mut sink = Sink::default();
    let result = output::buffered(
        &mut sink,
        |writer| {
            writer.write_all(b"uncommitted packet")?;
            Err(io::Error::other("injected serializer failure"))
        },
        |_| {},
    );
    assert!(result.is_err());
    assert_eq!(sink.writes, 0);
    assert!(sink.bytes.is_empty());

    let mut sink = Sink {
        flush_error: true,
        ..Sink::default()
    };
    let result = output::buffered(&mut sink, |writer| writer.write_all(b"trace"), |_| {});
    assert!(result.is_err());
    assert_eq!(sink.bytes, b"trace");
    assert_eq!(sink.writes, 1);
    assert_eq!(sink.flushes, 1);
}
