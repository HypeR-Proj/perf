// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Small write-only protobuf subset used by Perfetto's stable public messages.
//! Field numbers follow the linked upstream schemas; no generated SDK/runtime.

#[derive(Default)]
pub(super) struct Message(pub Vec<u8>);

impl Message {
    fn varint(&mut self, mut value: u64) {
        while value >= 128 {
            self.0.push(value as u8 | 0x80);
            value >>= 7;
        }
        self.0.push(value as u8);
    }
    pub fn number(&mut self, field: u32, value: u64) {
        self.varint(u64::from(field) << 3);
        self.varint(value);
    }
    pub fn bytes(&mut self, field: u32, value: &[u8]) {
        self.varint((u64::from(field) << 3) | 2);
        self.varint(value.len() as u64);
        self.0.extend_from_slice(value);
    }
    pub fn text(&mut self, field: u32, value: &str) {
        self.bytes(field, value.as_bytes());
    }
    pub fn message(&mut self, field: u32, value: Self) {
        self.bytes(field, &value.0);
    }
    pub fn fixed(&mut self, field: u32, value: u64) {
        self.varint((u64::from(field) << 3) | 1);
        self.0.extend_from_slice(&value.to_le_bytes());
    }
}
