// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Explainable, read-only ransomware-pattern assessment for one workload's activity window.
//!
//! The caller (a gateway, the I/O sensor, a SIEM) supplies per-interval activity samples; Atlas
//! scores the latest interval against a median/MAD baseline of the earlier ones plus a few
//! absolute tells (mass renames, high-entropy rewrites). The verdict never executes anything
//! (`can_execute: false`): it names the next step, e.g. `POST /snapshots/{id}/lock`.

use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use crate::auth::Actor;
use crate::state::AppState;
use atlas_common::{AppError, AppResult};

/// Intervals needed before the baseline is trusted.
const MIN_BASELINE: usize = 6;
const MAX_SAMPLES: usize = 10_000;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Sample {
    /// Bytes written in the interval.
    bytes_written: f64,
    /// Files created or overwritten in the interval.
    files_modified: f64,
    /// Files renamed in the interval (ransomware appends extensions).
    files_renamed: f64,
    /// Files deleted in the interval.
    files_deleted: f64,
    /// Share (0..=1) of written blocks that look encrypted/compressed (Shannon entropy > 7.5).
    high_entropy_ratio: f64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AssessRequest {
    workload: String,
    /// Oldest first; the last sample is the one being judged.
    samples: Vec<Sample>,
    /// 0.5 (quiet) … 2.0 (twitchy); default 1.0.
    #[serde(default = "one")]
    sensitivity: f64,
}

fn one() -> f64 {
    1.0
}

#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct Signal {
    name: &'static str,
    score: f64,
    detail: String,
}

#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct Assessment {
    workload: String,
    model: &'static str,
    /// `insufficient_data` | `normal` | `suspicious` | `likely_ransomware`.
    verdict: &'static str,
    risk_score: f64,
    signals: Vec<Signal>,
    recommended_actions: Vec<&'static str>,
    can_execute: bool,
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    match s.len() {
        0 => 0.0,
        n if n % 2 == 0 => (s[n / 2 - 1] + s[n / 2]) / 2.0,
        n => s[n / 2],
    }
}

/// Robust z-score of `current` against `baseline`; a flat baseline falls back to a relative change.
fn robust_score(baseline: &[f64], current: f64) -> f64 {
    let med = median(baseline);
    let mad = median(&baseline.iter().map(|x| (x - med).abs()).collect::<Vec<_>>());
    if mad > f64::EPSILON {
        0.674_489_75 * (current - med) / mad
    } else {
        (current - med) / med.abs().max(1.0)
    }
}

fn finite(s: &Sample) -> bool {
    [
        s.bytes_written,
        s.files_modified,
        s.files_renamed,
        s.files_deleted,
        s.high_entropy_ratio,
    ]
    .iter()
    .all(|x| x.is_finite() && *x >= 0.0)
        && s.high_entropy_ratio <= 1.0
}

pub(crate) fn assess(req: &AssessRequest) -> Assessment {
    let mut a = Assessment {
        workload: req.workload.clone(),
        model: "ransomware_mad_entropy_v1",
        verdict: "insufficient_data",
        risk_score: 0.0,
        signals: vec![],
        recommended_actions: vec![],
        can_execute: false,
    };
    if req.samples.len() <= MIN_BASELINE {
        return a;
    }
    let (cur, base) = req.samples.split_last().expect("checked length");
    let k = req.sensitivity.clamp(0.5, 2.0);
    let col = |f: fn(&Sample) -> f64| base.iter().map(f).collect::<Vec<_>>();

    // Each signal contributes 0..=1; the weights sum to 1.
    let mut push = |name: &'static str, weight: f64, raw: f64, scale: f64, detail: String| {
        let score = (raw * k / scale).clamp(0.0, 1.0);
        a.risk_score += weight * score;
        if score > 0.0 {
            a.signals.push(Signal {
                name,
                score,
                detail,
            });
        }
    };
    let z = robust_score(&col(|s| s.files_modified), cur.files_modified);
    push(
        "file_modification_burst",
        0.25,
        z,
        8.0,
        format!("{} files modified, robust z = {z:.1}", cur.files_modified),
    );
    let z = robust_score(&col(|s| s.files_renamed), cur.files_renamed);
    push(
        "mass_rename",
        0.25,
        z,
        8.0,
        format!("{} files renamed, robust z = {z:.1}", cur.files_renamed),
    );
    let z = robust_score(&col(|s| s.files_deleted), cur.files_deleted);
    push(
        "mass_delete",
        0.1,
        z,
        8.0,
        format!("{} files deleted, robust z = {z:.1}", cur.files_deleted),
    );
    let z = robust_score(&col(|s| s.bytes_written), cur.bytes_written);
    push(
        "write_volume_burst",
        0.1,
        z,
        8.0,
        format!("{} bytes written, robust z = {z:.1}", cur.bytes_written),
    );
    // Entropy is absolute: encrypted data is near-random whatever this workload normally writes,
    // but only counts when it is new relative to the baseline's own typical share.
    let base_entropy = median(&col(|s| s.high_entropy_ratio));
    let raw = (cur.high_entropy_ratio - base_entropy).max(0.0);
    push(
        "high_entropy_writes",
        0.3,
        raw,
        0.5,
        format!(
            "{:.0}% of written blocks look encrypted (baseline {:.0}%)",
            cur.high_entropy_ratio * 100.0,
            base_entropy * 100.0
        ),
    );
    a.risk_score = (a.risk_score * 100.0).round() / 100.0;

    // Entropy plus at least one behavioural tell is what separates ransomware from a backup or
    // media ingest, which are also bursty.
    let has = |n: &str| a.signals.iter().any(|s| s.name == n && s.score >= 0.5);
    let behavioural = has("file_modification_burst") || has("mass_rename") || has("mass_delete");
    a.verdict = if a.risk_score >= 0.6 && has("high_entropy_writes") && behavioural {
        "likely_ransomware"
    } else if a.risk_score >= 0.35 {
        "suspicious"
    } else {
        "normal"
    };
    a.recommended_actions = match a.verdict {
        "likely_ransomware" => vec![
            "Take and retention-lock a snapshot now (POST /snapshots/{id}/lock) before more data is overwritten",
            "Isolate the writing client or pause its share",
            "Do not delete older snapshots; restore from the last snapshot taken before the burst",
        ],
        "suspicious" => vec![
            "Take a retention-locked snapshot as a precaution",
            "Check whether a backup, migration or ingest job explains the burst",
        ],
        _ => vec![],
    };
    a
}

/// `POST /ai/ransomware/assess` — score one workload's recent activity. Read-only.
pub(crate) async fn ai_ransomware_assess(
    Extension(actor): Extension<Actor>,
    axum::extract::State(s): axum::extract::State<AppState>,
    Json(req): Json<AssessRequest>,
) -> AppResult<Json<Assessment>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    if req.workload.trim().is_empty() {
        return Err(AppError::Validation("workload is required".into()));
    }
    if req.samples.len() > MAX_SAMPLES || !req.samples.iter().all(finite) {
        return Err(AppError::Validation(format!(
            "samples must be at most {MAX_SAMPLES} finite, non-negative values \
             (high_entropy_ratio within 0..=1)"
        )));
    }
    if !(0.5..=2.0).contains(&req.sensitivity) {
        return Err(AppError::Validation(
            "sensitivity must be between 0.5 and 2.0".into(),
        ));
    }
    Ok(Json(assess(&req)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(m: f64, r: f64, d: f64, e: f64) -> Sample {
        Sample {
            bytes_written: m * 4096.0,
            files_modified: m,
            files_renamed: r,
            files_deleted: d,
            high_entropy_ratio: e,
        }
    }

    fn req(mut samples: Vec<Sample>, last: Sample) -> AssessRequest {
        samples.push(last);
        AssessRequest {
            workload: "w".into(),
            samples,
            sensitivity: 1.0,
        }
    }

    fn quiet() -> Vec<Sample> {
        (0..10)
            .map(|i| sample(20.0 + f64::from(i % 3), 1.0, 1.0, 0.05))
            .collect()
    }

    #[test]
    fn short_history_is_insufficient() {
        let r = req(quiet()[..3].to_vec(), sample(5000.0, 5000.0, 0.0, 0.99));
        assert_eq!(assess(&r).verdict, "insufficient_data");
    }

    #[test]
    fn steady_activity_is_normal() {
        let a = assess(&req(quiet(), sample(21.0, 1.0, 1.0, 0.05)));
        assert_eq!(a.verdict, "normal");
        assert!(a.recommended_actions.is_empty());
        assert!(!a.can_execute);
    }

    #[test]
    fn encrypt_and_rename_burst_is_flagged() {
        let a = assess(&req(quiet(), sample(4000.0, 3900.0, 10.0, 0.97)));
        assert_eq!(a.verdict, "likely_ransomware", "{a:?}");
        assert!(a.signals.iter().any(|s| s.name == "high_entropy_writes"));
        assert!(!a.recommended_actions.is_empty());
    }

    #[test]
    fn a_big_low_entropy_burst_is_not_ransomware() {
        // A backup or ingest job: bursty, but the data is not encrypted.
        let a = assess(&req(quiet(), sample(4000.0, 2.0, 0.0, 0.05)));
        assert_ne!(a.verdict, "likely_ransomware", "{a:?}");
    }

    #[test]
    fn high_entropy_alone_is_not_enough() {
        // A workload that starts writing compressed media, with no behavioural change.
        let a = assess(&req(quiet(), sample(21.0, 1.0, 1.0, 0.99)));
        assert_ne!(a.verdict, "likely_ransomware", "{a:?}");
    }

    #[test]
    fn a_noisy_baseline_does_not_alarm_on_normal_noise() {
        let noisy: Vec<Sample> = [5.0, 90.0, 12.0, 70.0, 8.0, 85.0, 15.0, 60.0]
            .iter()
            .map(|m| sample(*m, 2.0, 1.0, 0.05))
            .collect();
        assert_eq!(
            assess(&req(noisy, sample(80.0, 2.0, 1.0, 0.05))).verdict,
            "normal"
        );
    }
}
