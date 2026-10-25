// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0
//! Compact bounded collection; names and call-site metadata are interned in
//! userspace. Export sorts by monotonic time without assuming CPU read order.
use hyper_os::inspect::trace::{TraceInfo, TraceRecord};
use std::collections::BTreeMap;
#[derive(Clone, Debug)]
pub struct Site {
    pub category: String,
    pub name: String,
    pub fields: Vec<String>,
    pub source: String,
    pub line: u64,
}
#[derive(Clone, Debug)]
pub struct Event {
    pub time: u64,
    pub sequence: u64,
    pub cpu: usize,
    pub site: usize,
    pub values: [u64; 4],
}
#[derive(Clone, Debug)]
pub struct Loss {
    pub cpu: usize,
    pub overwritten: u64,
    pub incomplete: bool,
}
pub struct Capture {
    pub info: TraceInfo,
    pub sites: Vec<Site>,
    pub events: Vec<Event>,
    pub names: BTreeMap<u64, String>,
    pub losses: Vec<Loss>,
    pub limited: bool,
    pub limit: usize,
}
impl Capture {
    pub fn new(info: TraceInfo, limit: usize) -> Self {
        let losses = (0..info.cpu_count as usize)
            .map(|cpu| Loss {
                cpu,
                overwritten: 0,
                incomplete: false,
            })
            .collect();
        Self {
            info,
            sites: Vec::new(),
            events: Vec::new(),
            names: BTreeMap::new(),
            losses,
            limited: false,
            limit,
        }
    }
    pub fn add(
        &mut self,
        cpu: usize,
        record: TraceRecord<'_>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        if self.events.len() >= self.limit {
            self.limited = true;
            return Ok(false);
        }
        // A small site catalogue avoids allocating a search key per event.
        // Compare all metadata: source names can be truncated on the wire.
        let site = if let Some(site) = self.sites.iter().position(|site| {
            site.source == record.source
                && site.line == record.line
                && site.name == record.name
                && site.category == record.category.name()
                && site
                    .fields
                    .iter()
                    .map(String::as_str)
                    .eq(record.fields[..record.field_count]
                        .iter()
                        .map(|(name, _)| *name))
        }) {
            site
        } else {
            let site = self.sites.len();
            self.sites.push(Site {
                category: record.category.name().into(),
                name: record.name.into(),
                fields: record.fields[..record.field_count]
                    .iter()
                    .map(|(name, _)| (*name).into())
                    .collect(),
                source: record.source.into(),
                line: record.line,
            });
            site
        };
        if self.events.len() == self.events.capacity() {
            self.events
                .try_reserve((self.limit - self.events.len()).min(4096))?;
        }
        self.events.push(Event {
            time: record.timestamp_ns,
            sequence: record.sequence,
            cpu,
            site,
            values: record.fields.map(|(_, value)| value),
        });
        Ok(true)
    }
    pub fn value(&self, event: &Event, name: &str) -> Option<u64> {
        self.sites[event.site]
            .fields
            .iter()
            .position(|field| field == name)
            .map(|index| event.values[index])
    }
    pub fn loss(&self) -> bool {
        self.limited
            || self
                .losses
                .iter()
                .any(|l| l.overwritten != 0 || l.incomplete)
    }
    /// Pairing migrated work requires a lossless view of every online CPU.
    /// Missing prefixes/tails on one CPU can hide a return and a new invocation
    /// between two boundaries retained on another CPU.
    pub fn cross_cpu_spans(&self) -> bool {
        !self.loss()
            && (0..self.info.cpu_count as usize)
                .all(|cpu| self.info.cpu_mask[cpu / 8] & (1 << (cpu % 8)) != 0)
    }
    pub fn sort(&mut self) {
        self.events
            .sort_unstable_by_key(|e| (e.time, e.cpu, e.sequence));
    }
}
