// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Deterministic risk scoring that combines [`crate::rca`] results with an optional
//! capacity signal. Read-only, no side effects, no I/O.
//!
//! Queue pressure is read from the `cause=` notes the RCA emits, never inferred here, so the
//! RCA's validity guards (missing queue events, inconsistent counters, small samples) apply.

use atlas_api_types::IoRca;

/// Overall risk level derived from latency, queue pressure and optional capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    Healthy,
    Elevated,
    Critical,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            RiskLevel::Healthy => "healthy",
            RiskLevel::Elevated => "elevated",
            RiskLevel::Critical => "critical",
        }
    }
}

/// Simple capacity signal used by the scorer.
#[derive(Debug, Clone, Copy)]
pub struct CapacitySignal {
    pub used_percent: f64,
    pub days_to_full: Option<f64>,
}

const CAPACITY_ELEVATED_PERCENT: f64 = 75.0;
const CAPACITY_CRITICAL_PERCENT: f64 = 90.0;
const DAYS_TO_FULL_ELEVATED: f64 = 7.0;

fn queue_pressure(r: &IoRca) -> bool {
    r.notes.iter().any(|n| {
        n.contains("cause=scheduler_queue_pressure") || n.contains("cause=mixed_queue_and_service")
    })
}

fn rca_level(r: &IoRca) -> RiskLevel {
    match r.verdict.as_str() {
        // No completions or no evidence: never score on it.
        "unknown" => RiskLevel::Healthy,
        "critical_latency" => RiskLevel::Critical,
        _ if queue_pressure(r) => RiskLevel::Critical,
        "elevated_latency" => RiskLevel::Elevated,
        _ => RiskLevel::Healthy,
    }
}

fn capacity_level(cap: CapacitySignal) -> RiskLevel {
    // Out-of-range or NaN input is ignored rather than guessed at.
    if !(0.0..=100.0).contains(&cap.used_percent) {
        return RiskLevel::Healthy;
    }
    if cap.used_percent >= CAPACITY_CRITICAL_PERCENT {
        RiskLevel::Critical
    } else if cap.used_percent >= CAPACITY_ELEVATED_PERCENT
        || cap
            .days_to_full
            .is_some_and(|d| (0.0..DAYS_TO_FULL_ELEVATED).contains(&d))
    {
        RiskLevel::Elevated
    } else {
        RiskLevel::Healthy
    }
}

/// Score a set of RCA results plus an optional capacity signal into one risk level (the worst
/// of all inputs).
pub fn score(rca: &[IoRca], capacity: Option<CapacitySignal>) -> RiskLevel {
    rca.iter()
        .map(rca_level)
        .chain(capacity.map(capacity_level))
        .max()
        .unwrap_or(RiskLevel::Healthy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rca(verdict: &str, notes: &[&str]) -> IoRca {
        IoRca {
            volume_id: None,
            device: "rbd0".into(),
            verdict: verdict.into(),
            p50_us: 100,
            p99_us: 500,
            hottest_comm: None,
            notes: notes.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn cap(used_percent: f64, days_to_full: Option<f64>) -> Option<CapacitySignal> {
        Some(CapacitySignal {
            used_percent,
            days_to_full,
        })
    }

    #[test]
    fn healthy_when_nothing_bad() {
        assert_eq!(score(&[], None), RiskLevel::Healthy);
        assert_eq!(score(&[rca("healthy", &[])], None), RiskLevel::Healthy);
    }

    #[test]
    fn latency_verdicts_map_to_levels() {
        assert_eq!(
            score(&[rca("elevated_latency", &[])], None),
            RiskLevel::Elevated
        );
        assert_eq!(
            score(&[rca("critical_latency", &[])], None),
            RiskLevel::Critical
        );
    }

    #[test]
    fn queue_pressure_is_critical_even_with_fast_service() {
        for cause in ["scheduler_queue_pressure", "mixed_queue_and_service"] {
            let note = format!("cause={cause}; observational evidence");
            assert_eq!(
                score(&[rca("healthy", &[&note])], None),
                RiskLevel::Critical
            );
        }
        let note = "cause=device_service_latency; observational evidence";
        assert_eq!(score(&[rca("healthy", &[note])], None), RiskLevel::Healthy);
    }

    #[test]
    fn unknown_verdict_is_not_evidence() {
        let note = "cause=scheduler_queue_pressure";
        assert_eq!(score(&[rca("unknown", &[note])], None), RiskLevel::Healthy);
    }

    #[test]
    fn capacity_thresholds() {
        assert_eq!(score(&[], cap(92.0, None)), RiskLevel::Critical);
        assert_eq!(score(&[], cap(80.0, None)), RiskLevel::Elevated);
        assert_eq!(score(&[], cap(10.0, Some(5.0))), RiskLevel::Elevated);
        assert_eq!(score(&[], cap(10.0, Some(30.0))), RiskLevel::Healthy);
        assert_eq!(
            score(&[rca("healthy", &[])], cap(80.0, Some(5.0))),
            RiskLevel::Elevated
        );
    }

    #[test]
    fn capacity_never_lowers_an_rca_level() {
        assert_eq!(
            score(&[rca("critical_latency", &[])], cap(10.0, None)),
            RiskLevel::Critical
        );
    }

    #[test]
    fn invalid_capacity_is_ignored() {
        assert_eq!(score(&[], cap(f64::NAN, None)), RiskLevel::Healthy);
        assert_eq!(score(&[], cap(-5.0, None)), RiskLevel::Healthy);
        assert_eq!(score(&[], cap(250.0, None)), RiskLevel::Healthy);
        assert_eq!(score(&[], cap(10.0, Some(f64::NAN))), RiskLevel::Healthy);
        assert_eq!(score(&[], cap(10.0, Some(-1.0))), RiskLevel::Healthy);
    }
}
