// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! In-process log2-µs histograms with a hard cardinality cap.

use std::collections::HashMap;

use atlas_api_types::{BioEvent, IoHistogram, IoOp};

use crate::{CARDINALITY_CAP, HIST_BUCKETS};

#[derive(Debug, Default, Clone)]
struct Acc {
    buckets: [u64; HIST_BUCKETS],
    count: u64,
    sum_us: u64,
    bytes: u64,
    queued: u64,
    queue_sum_us: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Hk {
    dev: u64,
    op: IoOp,
}

#[derive(Debug, Default)]
pub struct HistSet {
    acc: HashMap<Hk, Acc>,
    dropped: u64,
}

impl HistSet {
    pub fn record(&mut self, ev: &BioEvent) {
        let hk = Hk {
            dev: ev.dev_key(),
            op: ev.op,
        };
        if !self.acc.contains_key(&hk) && self.acc.len() as u64 >= CARDINALITY_CAP {
            self.dropped += 1;
            return;
        }
        let a = self.acc.entry(hk).or_default();
        let us = ev.latency_us().max(1);
        let bucket = us.next_power_of_two().trailing_zeros() as usize;
        let bucket = bucket.min(HIST_BUCKETS - 1);
        a.buckets[bucket] += 1;
        a.count += 1;
        a.sum_us += ev.latency_us();
        a.bytes += ev.bytes as u64;
        if ev.queued_ns > 0 {
            a.queued += 1;
            a.queue_sum_us += ev.queue_us();
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn cardinality(&self) -> u64 {
        self.acc.len() as u64
    }

    pub fn snapshots(
        &self,
        name: impl Fn(u32, u32) -> (String, Option<String>),
    ) -> Vec<IoHistogram> {
        let mut out = Vec::new();
        for (hk, a) in &self.acc {
            let major = (hk.dev >> 32) as u32;
            let minor = (hk.dev & 0xffff_ffff) as u32;
            let (device, volume_id) = name(major, minor);
            out.push(IoHistogram {
                device,
                volume_id,
                op: hk.op,
                buckets: a.buckets.to_vec(),
                count: a.count,
                sum_us: a.sum_us,
                bytes: a.bytes,
                queued: a.queued,
                queue_sum_us: a.queue_sum_us,
            });
        }
        out.sort_by(|a, b| {
            a.device
                .cmp(&b.device)
                .then(a.op.as_str().cmp(b.op.as_str()))
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atlas_api_types::IoOp;

    fn ev(us: u64, op: IoOp) -> BioEvent {
        BioEvent {
            issued_ns: 0,
            completed_ns: us * 1_000,
            queued_ns: us * 500,
            bytes: 4096,
            op,
            major: 8,
            minor: 0,
            pid: 1,
            cgroup_id: 1,
            comm: "dd".into(),
        }
    }

    #[test]
    fn log2_bucket() {
        let mut h = HistSet::default();
        h.record(&ev(1, IoOp::Read));
        h.record(&ev(8, IoOp::Read));
        let snaps = h.snapshots(|_, _| ("sda".into(), None));
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].count, 2);
        assert!(snaps[0].buckets.iter().sum::<u64>() == 2);
        // 0.5µs rounds down to 0; 4µs of queue wait for the 8µs request.
        assert_eq!((snaps[0].queued, snaps[0].queue_sum_us), (2, 4));
    }

    #[test]
    fn p99_tracks_slow_tail() {
        let mut h = HistSet::default();
        for _ in 0..70 {
            h.record(&ev(4, IoOp::Write));
        }
        for _ in 0..30 {
            h.record(&ev(1 << 14, IoOp::Write));
        }
        let snap = &h.snapshots(|_, _| ("sda".into(), None))[0];
        assert!(snap.p99_us() >= 1 << 14);
        assert!(snap.p50_us() <= 8);
    }
}
