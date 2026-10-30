// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Read-only I/O shape per device, for spotting AI-style access (large checkpoint writes, bulk
//! dataset or model reads, small latency-sensitive reads).
//!
//! This is a heuristic over what the block layer reports: per device and op, the number of
//! requests, their bytes and the latency histogram. It cannot see periodicity, file names or which
//! framework issued the I/O, so it names a *shape*, never a job, and it lists the numbers behind
//! every call. It never acts on a verdict.

use atlas_api_types::{IoHistogram, IoOp};
use serde::Serialize;

/// Fewer completed requests than this and the shape is `unknown`.
const MIN_OPS: u64 = 20;
/// Average request size at or above which I/O counts as large and sequential-looking.
const LARGE_IO_BYTES: u64 = 1 << 20;
/// Average request size at or below which reads count as small.
const SMALL_IO_BYTES: u64 = 64 << 10;
/// One direction must carry at least this share of the bytes to define the shape.
const DOMINANT_PERCENT: u64 = 80;
/// p99 (µs) above which a large write or read is reported as slowed.
const SLOW_P99_US: u64 = 50_000;
/// p99 (µs) above which a small read is reported as slowed.
const SLOW_SMALL_READ_P99_US: u64 = 2_000;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct IoShape {
    pub device: String,
    pub volume_id: Option<String>,
    /// `large_write` (checkpoint-like), `bulk_read` (dataset or model load), `small_read`
    /// (latency-sensitive reads), `mixed` or `unknown`.
    pub shape: &'static str,
    pub read_ops: u64,
    pub write_ops: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub avg_read_bytes: u64,
    pub avg_write_bytes: u64,
    pub p99_read_us: u64,
    pub p99_write_us: u64,
    /// Set when the shape's own latency expectation is missed.
    pub finding: Option<String>,
    /// The numbers behind the verdict, so a reader can disagree with it.
    pub evidence: Vec<String>,
}

#[derive(Default)]
struct Side {
    ops: u64,
    bytes: u64,
    p99: u64,
}

fn side(h: Option<&IoHistogram>) -> Side {
    h.map(|h| Side {
        ops: h.count,
        bytes: h.bytes,
        p99: h.p99_us(),
    })
    .unwrap_or_default()
}

/// One entry per (device, volume) that has any read or write histogram.
pub fn classify(hists: &[IoHistogram]) -> Vec<IoShape> {
    let mut keys: Vec<(&str, Option<&str>)> = hists
        .iter()
        .filter(|h| matches!(h.op, IoOp::Read | IoOp::Write))
        .map(|h| (h.device.as_str(), h.volume_id.as_deref()))
        .collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .map(|(device, volume)| {
            let pick = |op: IoOp| {
                hists
                    .iter()
                    .find(|h| h.device == device && h.volume_id.as_deref() == volume && h.op == op)
            };
            shape_of(device, volume, side(pick(IoOp::Read)), side(pick(IoOp::Write)))
        })
        .collect()
}

fn shape_of(device: &str, volume: Option<&str>, r: Side, w: Side) -> IoShape {
    let avg = |s: &Side| s.bytes.checked_div(s.ops).unwrap_or(0);
    let (avg_r, avg_w) = (avg(&r), avg(&w));
    let total_bytes = r.bytes.saturating_add(w.bytes);
    let share = |part: u64| part.saturating_mul(100).checked_div(total_bytes).unwrap_or(0);
    let mut evidence = vec![
        format!("reads: {} ops, {} bytes, avg {} B, p99 {}µs", r.ops, r.bytes, avg_r, r.p99),
        format!("writes: {} ops, {} bytes, avg {} B, p99 {}µs", w.ops, w.bytes, avg_w, w.p99),
    ];
    let mut finding = None;
    let shape = if r.ops.saturating_add(w.ops) < MIN_OPS {
        evidence.push(format!("fewer than {MIN_OPS} completed requests"));
        "unknown"
    } else if share(w.bytes) >= DOMINANT_PERCENT && avg_w >= LARGE_IO_BYTES {
        evidence.push(format!("{}% of bytes are writes averaging at least 1 MiB", share(w.bytes)));
        if w.p99 >= SLOW_P99_US {
            finding = Some(format!(
                "large writes are slow: p99 {}µs; a training checkpoint on this device would stall",
                w.p99
            ));
        }
        "large_write"
    } else if share(r.bytes) >= DOMINANT_PERCENT && avg_r >= LARGE_IO_BYTES {
        evidence.push(format!("{}% of bytes are reads averaging at least 1 MiB", share(r.bytes)));
        if r.p99 >= SLOW_P99_US {
            finding = Some(format!(
                "large reads are slow: p99 {}µs; dataset or model loading would starve",
                r.p99
            ));
        }
        "bulk_read"
    } else if share(r.bytes) >= DOMINANT_PERCENT && r.ops > 0 && avg_r <= SMALL_IO_BYTES {
        evidence.push(format!("{}% of bytes are reads averaging at most 64 KiB", share(r.bytes)));
        if r.p99 >= SLOW_SMALL_READ_P99_US {
            finding = Some(format!(
                "small reads are slow: p99 {}µs; latency-sensitive readers would notice",
                r.p99
            ));
        }
        "small_read"
    } else {
        "mixed"
    };
    IoShape {
        device: device.to_string(),
        volume_id: volume.map(str::to_string),
        shape,
        read_ops: r.ops,
        write_ops: w.ops,
        read_bytes: r.bytes,
        write_bytes: w.bytes,
        avg_read_bytes: avg_r,
        avg_write_bytes: avg_w,
        p99_read_us: r.p99,
        p99_write_us: w.p99,
        finding,
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HIST_BUCKETS;

    /// `count` requests of `bytes_each`, all landing in the bucket for roughly `latency_us`.
    fn hist(op: IoOp, count: u64, bytes_each: u64, latency_us: u64) -> IoHistogram {
        let mut buckets = vec![0u64; HIST_BUCKETS];
        let idx = (64 - latency_us.max(1).leading_zeros() as usize - 1).min(HIST_BUCKETS - 1);
        buckets[idx] = count;
        IoHistogram {
            device: "nvme0n1".into(),
            volume_id: None,
            op,
            buckets,
            count,
            sum_us: count * latency_us,
            bytes: count * bytes_each,
            queued: 0,
            queue_sum_us: 0,
        }
    }

    fn one(hists: &[IoHistogram]) -> IoShape {
        let v = classify(hists);
        assert_eq!(v.len(), 1);
        v.into_iter().next().unwrap()
    }

    #[test]
    fn big_writes_are_large_write_and_slow_ones_are_flagged() {
        let ok = one(&[hist(IoOp::Write, 100, 4 << 20, 1_000)]);
        assert_eq!(ok.shape, "large_write");
        assert!(ok.finding.is_none());
        let slow = one(&[hist(IoOp::Write, 100, 4 << 20, 100_000)]);
        assert_eq!(slow.shape, "large_write");
        assert!(slow.finding.unwrap().contains("checkpoint"));
    }

    #[test]
    fn big_reads_are_bulk_read_and_small_reads_are_small_read() {
        assert_eq!(one(&[hist(IoOp::Read, 100, 2 << 20, 500)]).shape, "bulk_read");
        let small = one(&[hist(IoOp::Read, 1000, 4096, 100)]);
        assert_eq!(small.shape, "small_read");
        assert!(small.finding.is_none());
        let slow_small = one(&[hist(IoOp::Read, 1000, 4096, 5_000)]);
        assert!(slow_small.finding.unwrap().contains("small reads are slow"));
    }

    #[test]
    fn a_balanced_mix_and_too_few_requests_are_named_honestly() {
        let mixed = one(&[
            hist(IoOp::Read, 100, 128 << 10, 200),
            hist(IoOp::Write, 100, 128 << 10, 200),
        ]);
        assert_eq!(mixed.shape, "mixed");
        let few = one(&[hist(IoOp::Write, 5, 4 << 20, 1_000)]);
        assert_eq!(few.shape, "unknown");
        assert!(few.evidence.iter().any(|e| e.contains("fewer than")));
    }

    #[test]
    fn every_verdict_carries_its_numbers_and_devices_are_separate() {
        let mut other = hist(IoOp::Read, 100, 2 << 20, 500);
        other.device = "nvme1n1".into();
        let all = classify(&[hist(IoOp::Write, 100, 4 << 20, 1_000), other]);
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|s| s.evidence.len() >= 2));
        assert_eq!(all[0].device, "nvme0n1");
        assert_eq!(all[0].shape, "large_write");
        assert_eq!(all[1].shape, "bulk_read");
    }
}
