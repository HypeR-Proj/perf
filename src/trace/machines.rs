// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Optional fleet observations, separate from kernel trace authority. Resolve
//! newly observed vCPUs at most once per second, and keep retired VM identities.

use super::model::{Capture, VmThread};
use hyper_os::wait::{ObjectSignals, WaitItem, wait_many};
use hyper_os::{Error, OwnedHandle, Status, handle::ByteChannelObject};
use hyper_service::vm::trace;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub(super) struct Observer {
    endpoint: Option<OwnedHandle<ByteChannelObject>>,
    next: Instant,
    sampled: bool,
}
impl Observer {
    pub fn new(endpoint: Option<OwnedHandle<ByteChannelObject>>) -> Self {
        Self {
            endpoint,
            next: Instant::now(),
            sampled: false,
        }
    }
    pub fn refresh(&mut self, capture: &mut Capture) {
        if Instant::now() < self.next
            || self.sampled
                && capture
                    .vcpu_threads
                    .iter()
                    .all(|id| capture.vms.contains_key(id))
        {
            return;
        }
        self.finish(capture);
    }
    /// Resolve initial ownership before starting the bounded recording window.
    pub fn prepare(&mut self) -> BTreeMap<u64, VmThread> {
        let mut vms = BTreeMap::new();
        self.sample(&mut vms);
        vms
    }
    pub fn finish(&mut self, capture: &mut Capture) {
        self.sample(&mut capture.vms);
    }
    fn sample(&mut self, vms: &mut BTreeMap<u64, VmThread>) {
        self.next = Instant::now() + Duration::from_secs(1);
        self.sampled = true;
        if self.read(vms).is_err() {
            // A late reply is never interpreted as a later snapshot. Metadata
            // failure must not interrupt trace collection or grant control.
            self.endpoint = None;
        }
    }
    fn read(&self, vms: &mut BTreeMap<u64, VmThread>) -> hyper_os::Result<()> {
        let Some(endpoint) = self.endpoint.as_ref() else {
            return Ok(());
        };
        let channel = endpoint.as_byte_channel();
        channel.try_send(trace::REQUEST)?;
        let deadline = hyper_os::time::deadline_after(Duration::from_millis(350))?.as_raw();
        let mut bytes = vec![0; trace::MAX_BYTES];
        loop {
            match channel.try_receive(&mut bytes) {
                Ok(n) => {
                    let records = trace::decode(&bytes[..n]).ok_or(Error::InvalidResponse)?;
                    for record in records {
                        vms.entry(record.thread).or_insert(VmThread {
                            name: record.name,
                            vcpu: record.vcpu,
                        });
                    }
                    return Ok(());
                }
                Err(Error::Status(Status::WOULD_BLOCK)) => {
                    wait_many(
                        &[WaitItem::new(
                            endpoint.as_handle_ref(),
                            ObjectSignals::<ByteChannelObject>::READABLE
                                .union(ObjectSignals::<ByteChannelObject>::PEER_CLOSED),
                        )],
                        deadline,
                    )?;
                }
                Err(e) => return Err(e),
            }
        }
    }
}
