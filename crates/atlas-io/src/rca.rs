// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Deterministic, read-only RCA from histograms + workloads.

use atlas_api_types::{IoHistogram, IoOp, IoRca, IoWorkload};

const P99_WARN_US: u64 = 2_000;
const P99_CRIT_US: u64 = 10_000;
const QUEUE_HEAVY_RATIO: u128 = 4;
const MIN_CAUSE_SAMPLES: u64 = 20;

pub fn explain(
    hists: &[IoHistogram],
    workloads: &[IoWorkload],
    volume: Option<&str>,
) -> Vec<IoRca> {
    let mut out = Vec::new();
    for h in hists {
        if h.op != IoOp::Read && h.op != IoOp::Write {
            continue;
        }
        if let Some(v) = volume {
            if h.volume_id.as_deref() != Some(v) && h.device != v {
                continue;
            }
        }
        let p99 = h.p99_us();
        let p50 = h.p50_us();
        // The kernel records insert->issue separately from issue->complete. Never subtract
        // scheduler wait from the service histogram or describe an average as an end-to-end p99.
        let queue_valid = h.queued <= h.count && (h.queued > 0 || h.queue_sum_us == 0);
        let avg_queue = if h.count > 0 && queue_valid {
            h.queue_sum_us / h.count
        } else {
            0
        };
        let avg_service = h.sum_us.checked_div(h.count).unwrap_or(0);
        let avg_total = avg_service.saturating_add(avg_queue);
        let enough = h.count >= MIN_CAUSE_SAMPLES;
        let verdict = if h.count == 0 {
            "unknown"
        } else if p99 >= P99_CRIT_US || (enough && avg_total >= P99_CRIT_US) {
            "critical_latency"
        } else if p99 >= P99_WARN_US || (enough && avg_total >= P99_WARN_US) {
            "elevated_latency"
        } else {
            "healthy"
        };
        let hottest = workloads
            .iter()
            .filter(|w| {
                w.device == h.device
                    && h.volume_id
                        .as_ref()
                        .is_none_or(|v| w.volume_id.as_ref() == Some(v))
            })
            .max_by_key(|w| w.bytes)
            .map(|w| w.comm.clone());
        let mut notes = vec![format!(
            "{} {} IOs, p50={}µs p99={}µs, {} bytes",
            h.count,
            h.op.as_str(),
            p50,
            p99,
            h.bytes
        )];
        notes.push(format!(
            "issue-to-complete service mean={}µs; p50/p99 exclude scheduler wait",
            avg_service
        ));
        if !queue_valid {
            notes.push("cause=unknown: inconsistent queue counters; queue evidence ignored".into());
        } else if !enough {
            notes.push(format!(
                "cause=unknown: fewer than {MIN_CAUSE_SAMPLES} completed requests"
            ));
        } else if h.queued == 0 {
            notes.push("scheduler queue not observed (bypass or missing insert events); no queue cause inferred".into());
        } else {
            notes.push(format!("scheduler queue observed on {}/{} requests; queue mean over all completed requests={}µs; mean total={}µs (not a percentile)", h.queued, h.count, avg_queue, avg_total));
            let queue_heavy = avg_queue >= P99_WARN_US
                && u128::from(h.queue_sum_us) >= u128::from(h.sum_us) * QUEUE_HEAVY_RATIO;
            let cause = match (queue_heavy, p99 >= P99_WARN_US) {
                (true, true) => "mixed_queue_and_service",
                (true, false) => "scheduler_queue_pressure",
                (false, true) => "device_service_latency",
                (false, false) => "no_latency_pressure",
            };
            notes.push(format!(
                "cause={cause}; observational evidence, not proof of a specific failing component"
            ));
        }
        if let Some(comm) = &hottest {
            notes.push(format!("hottest issuer on {} is {comm}", h.device));
        }
        if p99 >= P99_WARN_US && p99 > p50.saturating_mul(8) {
            notes.push(
                "long tail vs median — check OSD/device saturation or a clone/discard job".into(),
            );
        }
        out.push(IoRca {
            volume_id: h.volume_id.clone(),
            device: h.device.clone(),
            verdict: verdict.into(),
            p50_us: p50,
            p99_us: p99,
            hottest_comm: hottest,
            notes,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use atlas_api_types::IoHistogram;

    #[test]
    fn flags_slow_writes() {
        let mut buckets = vec![0u64; 26];
        buckets[14] = 20; // 16ms
        let h = IoHistogram {
            device: "rbd0".into(),
            volume_id: Some("vol_vm_web01".into()),
            op: IoOp::Write,
            buckets,
            count: 20,
            sum_us: 20 * 16_000,
            bytes: 20 * 128 * 1024,
            queued: 0,
            queue_sum_us: 0,
        };
        let rca = explain(&[h], &[], None);
        assert_eq!(rca[0].verdict, "critical_latency");
    }
}

#[cfg(test)]
mod queue_scenarios {
    use super::*;

    fn histogram(service_us: u64, queue_us: u64) -> IoHistogram {
        let mut buckets = vec![0; 26];
        buckets[service_us.max(1).next_power_of_two().trailing_zeros() as usize] = 100;
        IoHistogram {
            device: "nvme0n1".into(),
            volume_id: Some("vol_orders".into()),
            op: IoOp::Read,
            buckets,
            count: 100,
            sum_us: service_us * 100,
            bytes: 409_600,
            queued: if queue_us > 0 { 100 } else { 0 },
            queue_sum_us: queue_us * 100,
        }
    }

    fn diagnosis(h: IoHistogram) -> IoRca {
        explain(&[h], &[], None).remove(0)
    }

    #[test]
    fn queue_pressure_is_visible_even_with_fast_service() {
        let r = diagnosis(histogram(100, 12_000));
        assert_eq!(r.verdict, "critical_latency");
        assert!(r.p99_us < 2_000, "p99 remains service-only");
        assert!(r
            .notes
            .iter()
            .any(|s| s.contains("cause=scheduler_queue_pressure")));
        assert!(r.notes.iter().any(|s| s.contains("not a percentile")));
    }

    #[test]
    fn slow_device_and_mixed_pressure_are_distinguished() {
        let device = diagnosis(histogram(8_000, 100));
        assert!(device
            .notes
            .iter()
            .any(|s| s.contains("cause=device_service_latency")));
        let mixed = diagnosis(histogram(8_000, 40_000));
        assert!(mixed
            .notes
            .iter()
            .any(|s| s.contains("cause=mixed_queue_and_service")));
    }

    #[test]
    fn missing_queue_events_are_not_evidence_of_zero_wait() {
        let r = diagnosis(histogram(100, 0));
        assert_eq!(r.verdict, "healthy");
        assert!(r.notes.iter().any(|s| s.contains("queue not observed")));
        assert!(!r.notes.iter().any(|s| s.contains("cause=scheduler")));
    }

    #[test]
    fn invalid_queue_counters_and_small_samples_do_not_infer_cause() {
        let mut h = histogram(100, 12_000);
        h.queued = 101;
        let r = diagnosis(h);
        assert_eq!(r.verdict, "healthy");
        assert!(r
            .notes
            .iter()
            .any(|s| s.contains("inconsistent queue counters")));
        let mut h = histogram(100, 12_000);
        h.count = 1;
        h.queued = 1;
        let r = diagnosis(h);
        assert!(r.notes.iter().any(|s| s.contains("fewer than 20")));
        assert!(!r.notes.iter().any(|s| s.contains("cause=scheduler")));
    }

    #[test]
    fn hottest_issuer_respects_volume_attribution() {
        let h = histogram(100, 12_000);
        let workloads = vec![
            IoWorkload {
                cgroup_id: 1,
                pid: 1,
                comm: "orders".into(),
                device: h.device.clone(),
                volume_id: h.volume_id.clone(),
                ops: 10,
                bytes: 100,
                sum_us: 100,
            },
            IoWorkload {
                cgroup_id: 2,
                pid: 2,
                comm: "unrelated".into(),
                device: h.device.clone(),
                volume_id: Some("other".into()),
                ops: 100,
                bytes: 1_000_000,
                sum_us: 100,
            },
        ];
        let r = explain(&[h], &workloads, Some("vol_orders"));
        assert_eq!(r[0].hottest_comm.as_deref(), Some("orders"));
        assert!(explain(&[histogram(100, 0)], &workloads, Some("missing")).is_empty());
    }

    #[test]
    fn empty_histogram_and_maximum_counters_are_safe() {
        let mut empty = histogram(1, 0);
        empty.count = 0;
        empty.sum_us = 0;
        let r = diagnosis(empty);
        assert_eq!(r.verdict, "unknown");
        let mut maximum = histogram(100, 1);
        maximum.sum_us = u64::MAX;
        maximum.queue_sum_us = u64::MAX;
        assert_eq!(diagnosis(maximum).verdict, "critical_latency");
    }
}
