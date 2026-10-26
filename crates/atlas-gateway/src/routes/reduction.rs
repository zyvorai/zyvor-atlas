// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Savings report for atlas-native compression and extent sharing (`docs/NATIVE_DATA_REDUCTION.md`).
//!
//! The node reports raw counters cluster-wide; this adds the ratios an operator reads. Sharing
//! covers every reference to an extent (snapshots and clones as well as deduplication), so it is
//! not a pure dedup figure.

use axum::{extract::State, Extension, Json};
use serde_json::{json, Value};

use crate::auth::Actor;
use crate::startup::NATIVE_BACKEND_ID;
use crate::state::AppState;
use atlas_common::{AppError, AppResult};

fn counter(v: &Value, key: &str) -> AppResult<u64> {
    v[key]
        .as_u64()
        .ok_or_else(|| AppError::Driver(format!("bad reduction report from the node: no {key}")))
}

fn ratio(num: u64, den: u64) -> f64 {
    if den == 0 {
        1.0
    } else {
        num as f64 / den as f64
    }
}

/// Raw node counters plus derived savings. Errors if a counter is missing rather than guessing.
pub(crate) fn report(raw: &Value) -> AppResult<Value> {
    // The node reports `{"groups": [...], "total": {...}}`; the totals are the cluster-wide figures.
    // A grouped report without an object `total` is a node/gateway mismatch: say so instead of
    // reading the top-level object and reporting a misleading missing counter.
    let raw = match raw.get("total") {
        Some(t) if t.is_object() => t,
        _ if raw.get("groups").is_some() => {
            return Err(AppError::Driver(
                "bad reduction report from the node: grouped report has no object total".into(),
            ))
        }
        _ => raw,
    };
    let logical = counter(raw, "logical_bytes")?;
    let referenced = counter(raw, "referenced_bytes")?;
    let stored = counter(raw, "stored_bytes")?;
    Ok(json!({
        "extents": counter(raw, "extents")?,
        "compressed_extents": counter(raw, "compressed_extents")?,
        "shared_extents": counter(raw, "shared_extents")?,
        "logical_bytes": logical,
        "referenced_bytes": referenced,
        "stored_bytes": stored,
        "saved_by_compression_bytes": logical.saturating_sub(stored),
        "saved_by_sharing_bytes": referenced.saturating_sub(logical),
        "compression_ratio": ratio(logical, stored),
        "sharing_ratio": ratio(referenced, logical),
        "note": "stored_bytes is one copy; replicas or erasure coding multiply it. Sharing includes snapshots and clones.",
    }))
}

/// `GET /native/reduction` — cluster-wide savings from atlas-native. Read-only.
pub(crate) async fn native_reduction(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> AppResult<Json<Value>> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_OPERATOR)?;
    let driver = s
        .drivers
        .get(NATIVE_BACKEND_ID)
        .ok_or_else(|| AppError::Unavailable("the atlas-native driver is not enabled".into()))?;
    let raw = driver
        .data_reduction()
        .await
        .map_err(super::volumes::driver_error)?;
    Ok(Json(report(&raw)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(logical: u64, referenced: u64, stored: u64) -> Value {
        json!({ "extents": 3, "compressed_extents": 2, "shared_extents": 1,
                "logical_bytes": logical, "referenced_bytes": referenced, "stored_bytes": stored })
    }

    #[test]
    fn derives_ratios_from_counters() {
        let r = report(&raw(1000, 3000, 250)).unwrap();
        assert_eq!(r["saved_by_compression_bytes"], 750);
        assert_eq!(r["saved_by_sharing_bytes"], 2000);
        assert_eq!(r["compression_ratio"], 4.0);
        assert_eq!(r["sharing_ratio"], 3.0);
    }

    #[test]
    fn reads_the_node_total_from_the_grouped_report() {
        let r = report(&json!({ "groups": [], "total": raw(1000, 3000, 250) })).unwrap();
        assert_eq!(r["compression_ratio"], 4.0);
    }

    #[test]
    fn grouped_report_without_an_object_total_is_an_error() {
        assert!(report(&json!({ "groups": [] })).is_err());
        assert!(report(&json!({ "groups": [], "total": null })).is_err());
    }

    #[test]
    fn flat_report_still_works() {
        assert_eq!(report(&raw(1000, 3000, 250)).unwrap()["sharing_ratio"], 3.0);
    }

    #[test]
    fn empty_cluster_reports_no_savings() {
        let r = report(&raw(0, 0, 0)).unwrap();
        assert_eq!(r["compression_ratio"], 1.0);
        assert_eq!(r["sharing_ratio"], 1.0);
    }

    #[test]
    fn missing_counter_is_an_error_not_a_guess() {
        assert!(report(&json!({ "extents": 1 })).is_err());
    }
}
