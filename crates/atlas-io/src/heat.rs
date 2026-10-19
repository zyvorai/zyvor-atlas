// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Per-minute latency heatmap columns for the Observatory: for each (device, op), a ring of
//! one-minute log2-µs bucket counts. The cumulative histograms answer "what is the distribution
//! since start"; this answers "when did it get slow".

use std::collections::{HashMap, VecDeque};

use atlas_api_types::{BioEvent, IoOp};
use serde::Serialize;

use crate::{CARDINALITY_CAP, HIST_BUCKETS};

/// Seconds per column.
pub const COLUMN_SECS: u64 = 60;
/// Columns kept per series (one hour).
pub const MAX_COLUMNS: usize = 60;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Key {
    dev: u64,
    op: IoOp,
}

#[derive(Debug, Default)]
pub struct HeatSet {
    series: HashMap<Key, VecDeque<(u64, [u64; HIST_BUCKETS])>>,
    dropped: u64,
}

/// One minute of one series.
#[derive(Debug, Clone, Serialize)]
pub struct HeatColumn {
    /// Start of the minute, Unix seconds.
    pub start_unix: u64,
    /// Log2-µs bucket counts, like `IoHistogram::buckets`.
    pub buckets: Vec<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IoHeatmap {
    pub device: String,
    pub volume_id: Option<String>,
    pub op: IoOp,
    pub column_secs: u64,
    /// Oldest first; minutes with no I/O are absent, not zero-filled.
    pub columns: Vec<HeatColumn>,
}

impl HeatSet {
    /// `now_unix` is when the event was collected (device timestamps are monotonic, not wall time).
    pub fn record(&mut self, ev: &BioEvent, now_unix: u64) {
        let key = Key {
            dev: ev.dev_key(),
            op: ev.op,
        };
        if !self.series.contains_key(&key) && self.series.len() as u64 >= CARDINALITY_CAP {
            self.dropped += 1;
            return;
        }
        let minute = now_unix - now_unix % COLUMN_SECS;
        let bucket = (ev.latency_us().max(1).next_power_of_two().trailing_zeros() as usize)
            .min(HIST_BUCKETS - 1);
        let ring = self.series.entry(key).or_default();
        match ring.back_mut() {
            Some((m, b)) if *m == minute => b[bucket] += 1,
            _ => {
                let mut b = [0; HIST_BUCKETS];
                b[bucket] = 1;
                ring.push_back((minute, b));
                while ring.len() > MAX_COLUMNS {
                    ring.pop_front();
                }
            }
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Columns newer than `since_unix`, sorted by device then op.
    pub fn snapshots(
        &self,
        since_unix: u64,
        name: impl Fn(u32, u32) -> (String, Option<String>),
    ) -> Vec<IoHeatmap> {
        let mut out: Vec<_> = self
            .series
            .iter()
            .filter_map(|(k, ring)| {
                let columns: Vec<_> = ring
                    .iter()
                    .filter(|(m, _)| *m + COLUMN_SECS > since_unix)
                    .map(|(m, b)| HeatColumn {
                        start_unix: *m,
                        buckets: b.to_vec(),
                    })
                    .collect();
                if columns.is_empty() {
                    return None;
                }
                let (device, volume_id) = name((k.dev >> 32) as u32, (k.dev & 0xffff_ffff) as u32);
                Some(IoHeatmap {
                    device,
                    volume_id,
                    op: k.op,
                    column_secs: COLUMN_SECS,
                    columns,
                })
            })
            .collect();
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

    fn ev(dev: u32, us: u64) -> BioEvent {
        BioEvent {
            issued_ns: 0,
            completed_ns: us * 1_000,
            queued_ns: 0,
            bytes: 4096,
            op: IoOp::Write,
            major: 8,
            minor: dev,
            pid: 1,
            cgroup_id: 1,
            comm: "x".into(),
        }
    }

    fn name(_: u32, minor: u32) -> (String, Option<String>) {
        (format!("sd{minor}"), None)
    }

    #[test]
    fn same_minute_accumulates_and_a_new_minute_adds_a_column() {
        let mut h = HeatSet::default();
        h.record(&ev(0, 100), 600);
        h.record(&ev(0, 100), 659);
        h.record(&ev(0, 100), 660);
        let s = h.snapshots(0, name);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].columns.len(), 2);
        assert_eq!(s[0].columns[0].start_unix, 600);
        assert_eq!(s[0].columns[0].buckets.iter().sum::<u64>(), 2);
        assert_eq!(s[0].columns[1].buckets.iter().sum::<u64>(), 1);
        // 100 µs falls in the 128 µs bucket (2^7).
        assert_eq!(s[0].columns[0].buckets[7], 2);
    }

    #[test]
    fn ring_keeps_only_the_last_hour() {
        let mut h = HeatSet::default();
        for i in 0..(MAX_COLUMNS as u64 + 5) {
            h.record(&ev(0, 10), i * COLUMN_SECS);
        }
        let s = h.snapshots(0, name);
        assert_eq!(s[0].columns.len(), MAX_COLUMNS);
        assert_eq!(s[0].columns[0].start_unix, 5 * COLUMN_SECS);
    }

    #[test]
    fn since_filters_old_columns_and_drops_empty_series() {
        let mut h = HeatSet::default();
        h.record(&ev(0, 10), 0);
        h.record(&ev(1, 10), 600);
        let s = h.snapshots(300, name);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].device, "sd1");
    }

    #[test]
    fn series_cardinality_is_capped() {
        let mut h = HeatSet::default();
        for dev in 0..(CARDINALITY_CAP as u32 + 10) {
            h.record(&ev(dev, 10), 0);
        }
        assert_eq!(h.snapshots(0, name).len() as u64, CARDINALITY_CAP);
        assert_eq!(h.dropped(), 10);
    }
}
