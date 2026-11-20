// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Collapses raw `ceph status`/`osd tree`/`osd df` JSON into a 5-value operator-facing severity
//! (`ClusterHealthState`), so a top-nav badge or an ops dashboard doesn't have to parse Ceph's own
//! vocabulary of health checks and PG-state strings. This is Atlas's own heuristic classification
//! for UX, not a Ceph API concept — thresholds below (85% OSD utilization, 2+ OSDs down, etc.) are
//! starting points, not tuned against real production incident data yet.

use atlas_api_types::ClusterHealthState;
use atlas_driver_core::{DriverError, StorageDriver};
use serde::Serialize;
use serde_json::Value;

/// OSD utilization at/above this percent is treated as "At Risk" — matches the threshold already
/// used client-side (`views/Ceph.tsx`'s `utilColor`) and in `atlas-monitor`'s pool-capacity alert
/// (`POOL_CRITICAL = 0.85`, `crates/atlas-monitor/src/lib.rs`).
const AT_RISK_UTILIZATION_PCT: f64 = 85.0;

#[derive(Debug, Clone, Serialize)]
pub struct HealthRollup {
    pub state: ClusterHealthState,
    /// One-line human summary, e.g. "1 OSD down, redundancy reduced".
    pub summary: String,
    /// Ordered list of contributing factors, most-severe first.
    pub reasons: Vec<String>,
    /// Raw `HEALTH_OK`/`HEALTH_WARN`/`HEALTH_ERR` passthrough for callers that want it.
    pub raw_status: String,
    pub osds_up: i64,
    pub osds_in: i64,
    pub osds_total: i64,
    pub pgs_total: i64,
    pub pgs_not_clean: i64,
    pub recovering: bool,
    /// Which health sources contributed to this rollup: `["cli"]` from the `ceph`/`rbd` CLI path
    /// alone, or `["cli", "rook-crd"]` once `merge_rook_health` has cross-checked it against a
    /// live `CephCluster.status.ceph.health` from the Kubernetes API.
    #[serde(default = "default_sources")]
    pub sources: Vec<String>,
    /// Set only when the CLI-derived state and the Rook CRD's own health disagree — surfaced
    /// rather than silently picking one, since a real disagreement (e.g. the CLI can't reach the
    /// cluster but Rook reports it healthy, or vice versa) is itself diagnostic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rook_disagreement: Option<String>,
}

fn default_sources() -> Vec<String> {
    vec!["cli".to_string()]
}

/// Fetch `ceph_status`/`ceph_osd_tree`/`ceph_osd_df` concurrently and classify them.
pub async fn compute(driver: &dyn StorageDriver) -> Result<HealthRollup, DriverError> {
    let (status, osd_tree, osd_df) = tokio::try_join!(
        driver.ceph_status(),
        driver.ceph_osd_tree(),
        driver.ceph_osd_df(),
    )?;
    Ok(classify(&status, &osd_tree, &osd_df))
}

/// Pure, synchronous classification over already-fetched JSON — unit-testable without a driver.
/// `osd_tree` is accepted (and fetched by [`compute`]) for future CRUSH-topology-aware rules
/// (e.g. an entire failure domain down) but isn't consulted by the current rule set.
pub fn classify(status: &Value, _osd_tree: &Value, osd_df: &Value) -> HealthRollup {
    let raw_status = status["health"]["status"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let osds_total = status["osdmap"]["num_osds"].as_i64().unwrap_or(0);
    let osds_up = status["osdmap"]["num_up_osds"].as_i64().unwrap_or(0);
    let osds_in = status["osdmap"]["num_in_osds"].as_i64().unwrap_or(0);
    let pgs_total = status["pgmap"]["num_pgs"].as_i64().unwrap_or(0);
    let recovering_bytes = status["pgmap"]["recovering_bytes_per_sec"]
        .as_f64()
        .unwrap_or(0.0);

    let pg_states: Vec<(String, i64)> = status["pgmap"]["pgs_by_state"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    (
                        e["state_name"].as_str().unwrap_or("").to_string(),
                        e["count"].as_i64().unwrap_or(0),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let pg_contains = |needle: &str| pg_states.iter().any(|(s, _)| s.contains(needle));
    let pgs_not_clean: i64 = pg_states
        .iter()
        .filter(|(s, _)| s != "active+clean")
        .map(|(_, c)| c)
        .sum();

    let checks: Vec<(String, String)> = status["health"]["checks"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(name, v)| {
                    (
                        name.clone(),
                        v["severity"].as_str().unwrap_or("").to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let has_check_severity = |sev: &str| checks.iter().any(|(_, s)| s == sev);
    let has_check_named = |name: &str| checks.iter().any(|(n, _)| n == name);

    let max_osd_util = osd_df["nodes"]
        .as_array()
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|n| n["utilization"].as_f64())
                .fold(0.0_f64, f64::max)
        })
        .unwrap_or(0.0);

    let recovering = recovering_bytes > 0.0
        || pg_contains("recovering")
        || pg_contains("backfilling")
        || pg_contains("backfill_wait")
        || pg_contains("recovery_wait");

    let mut reasons: Vec<String> = Vec::new();

    // Critical: cluster-reported error, unavailable data, or every OSD down.
    let critical = raw_status == "HEALTH_ERR"
        || pg_contains("down")
        || pg_contains("incomplete")
        || pg_contains("stale")
        || has_check_severity("HEALTH_ERR")
        || (osds_total > 0 && osds_up == 0);
    if critical {
        if raw_status == "HEALTH_ERR" {
            reasons.push("cluster reports HEALTH_ERR".into());
        }
        if pg_contains("down") || pg_contains("incomplete") || pg_contains("stale") {
            reasons.push("placement groups unavailable (down/incomplete/stale)".into());
        }
        if has_check_severity("HEALTH_ERR") {
            reasons.push("one or more HEALTH_ERR checks active".into());
        }
        if osds_total > 0 && osds_up == 0 {
            reasons.push("no OSDs are up".into());
        }
        return finish(
            ClusterHealthState::Critical,
            reasons,
            raw_status,
            osds_up,
            osds_in,
            osds_total,
            pgs_total,
            pgs_not_clean,
            recovering,
        );
    }

    // At Risk: capacity pressure or degraded fault tolerance, not yet an outage.
    let osds_down = osds_total - osds_up;
    let at_risk = max_osd_util >= AT_RISK_UTILIZATION_PCT
        || has_check_named("NEARFULL")
        || has_check_named("BACKFILLFULL")
        || pg_contains("peering")
        || pg_contains("inactive")
        || pg_contains("unknown")
        || osds_down >= 2
        || osds_in < osds_total;
    if at_risk {
        if max_osd_util >= AT_RISK_UTILIZATION_PCT {
            reasons.push(format!(
                "an OSD is {max_osd_util:.1}% utilized (>= {AT_RISK_UTILIZATION_PCT}%)"
            ));
        }
        if has_check_named("NEARFULL") || has_check_named("BACKFILLFULL") {
            reasons.push("cluster is near capacity (NEARFULL/BACKFILLFULL)".into());
        }
        if pg_contains("peering") || pg_contains("inactive") || pg_contains("unknown") {
            reasons.push("placement groups peering/inactive/unknown".into());
        }
        if osds_down >= 2 {
            reasons.push(format!("{osds_down} OSDs are down"));
        }
        if osds_in < osds_total {
            reasons.push(format!("{} OSD(s) marked out", osds_total - osds_in));
        }
        return finish(
            ClusterHealthState::AtRisk,
            reasons,
            raw_status,
            osds_up,
            osds_in,
            osds_total,
            pgs_total,
            pgs_not_clean,
            recovering,
        );
    }

    // Rebuilding: an active recovery/backfill is under way but nothing above fired.
    if recovering {
        reasons.push("recovery/backfill in progress".into());
        return finish(
            ClusterHealthState::Rebuilding,
            reasons,
            raw_status,
            osds_up,
            osds_in,
            osds_total,
            pgs_total,
            pgs_not_clean,
            recovering,
        );
    }

    // Degraded: a warning-level condition with no more specific classification above.
    let degraded = raw_status == "HEALTH_WARN"
        || pg_contains("degraded")
        || pg_contains("undersized")
        || osds_down == 1;
    if degraded {
        if raw_status == "HEALTH_WARN" {
            reasons.push("cluster reports HEALTH_WARN".into());
        }
        if pg_contains("degraded") || pg_contains("undersized") {
            reasons.push("placement groups degraded/undersized".into());
        }
        if osds_down == 1 {
            reasons.push("1 OSD is down".into());
        }
        return finish(
            ClusterHealthState::Degraded,
            reasons,
            raw_status,
            osds_up,
            osds_in,
            osds_total,
            pgs_total,
            pgs_not_clean,
            recovering,
        );
    }

    finish(
        ClusterHealthState::Healthy,
        vec!["all checks nominal".into()],
        raw_status,
        osds_up,
        osds_in,
        osds_total,
        pgs_total,
        pgs_not_clean,
        recovering,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish(
    state: ClusterHealthState,
    reasons: Vec<String>,
    raw_status: String,
    osds_up: i64,
    osds_in: i64,
    osds_total: i64,
    pgs_total: i64,
    pgs_not_clean: i64,
    recovering: bool,
) -> HealthRollup {
    let summary = reasons
        .first()
        .cloned()
        .unwrap_or_else(|| "no signal".into());
    HealthRollup {
        state,
        summary,
        reasons,
        raw_status,
        osds_up,
        osds_in,
        osds_total,
        pgs_total,
        pgs_not_clean,
        recovering,
        sources: default_sources(),
        rook_disagreement: None,
    }
}

/// Cross-check a CLI-derived rollup against `CephCluster.status.ceph.health` read straight from
/// the Kubernetes API (Rook's own view — reachable even when the `ceph` CLI/keyring isn't). Called
/// by the gateway, which owns the k8s driver; `atlas-driver-ceph` itself never talks to Kubernetes,
/// so this only *merges* an already-fetched value rather than fetching one.
///
/// `rook_health` is `CephCluster.status.ceph.health` (`"HEALTH_OK"`/`"HEALTH_WARN"`/
/// `"HEALTH_ERR"`), when the CR and its status are present. A mismatch doesn't change `state` —
/// the CLI-derived classification is left as the primary signal — it's recorded in
/// `rook_disagreement` so callers can surface it rather than silently trusting one source.
pub fn merge_rook_health(rollup: &mut HealthRollup, rook_health: Option<&str>) {
    let Some(rook_health) = rook_health else {
        return;
    };
    rollup.sources.push("rook-crd".to_string());
    if rook_health != rollup.raw_status {
        rollup.rook_disagreement = Some(format!(
            "cli reports {} but Rook's CephCluster status reports {rook_health}",
            rollup.raw_status
        ));
    }
}

/// A coarse rollup built from `CephCluster.status.ceph.health` alone, for when the `ceph` CLI is
/// unreachable (no keyring/socket on the gateway pod) but the Rook CR is — since Rook always has
/// API-server access to the cluster it manages. Coarser than [`classify`] (no PG/OSD-utilization
/// detail, since only the top-level health string is available), but keeps `/ceph/health-rollup`
/// answering instead of 502ing outright.
pub fn from_rook_only(rook_health: &str) -> HealthRollup {
    let state = match rook_health {
        "HEALTH_OK" => ClusterHealthState::Healthy,
        "HEALTH_WARN" => ClusterHealthState::Degraded,
        "HEALTH_ERR" => ClusterHealthState::Critical,
        _ => ClusterHealthState::AtRisk,
    };
    HealthRollup {
        state,
        summary: format!("Rook CephCluster reports {rook_health} (ceph CLI unreachable)"),
        reasons: vec![format!("Rook CephCluster status: {rook_health}")],
        raw_status: rook_health.to_string(),
        osds_up: 0,
        osds_in: 0,
        osds_total: 0,
        pgs_total: 0,
        pgs_not_clean: 0,
        recovering: false,
        sources: vec!["rook-crd".to_string()],
        rook_disagreement: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status(health: &str, checks: Value, osdmap: Value, pgmap: Value) -> Value {
        json!({ "health": { "status": health, "checks": checks }, "osdmap": osdmap, "pgmap": pgmap })
    }

    fn osd_df(utils: &[f64]) -> Value {
        json!({ "nodes": utils.iter().map(|u| json!({ "utilization": u })).collect::<Vec<_>>() })
    }

    fn empty_tree() -> Value {
        json!({ "nodes": [] })
    }

    #[test]
    fn classifies_healthy() {
        let s = status(
            "HEALTH_OK",
            json!({}),
            json!({ "num_osds": 6, "num_up_osds": 6, "num_in_osds": 6 }),
            json!({ "num_pgs": 100, "pgs_by_state": [{"state_name": "active+clean", "count": 100}], "recovering_bytes_per_sec": 0 }),
        );
        let r = classify(&s, &empty_tree(), &osd_df(&[10.0, 20.0]));
        assert_eq!(r.state, ClusterHealthState::Healthy);
    }

    #[test]
    fn classifies_degraded_on_health_warn() {
        let s = status(
            "HEALTH_WARN",
            json!({ "OSD_DOWN": { "severity": "HEALTH_WARN", "summary": { "message": "1 osds down" } } }),
            json!({ "num_osds": 6, "num_up_osds": 5, "num_in_osds": 6 }),
            json!({ "num_pgs": 289, "pgs_by_state": [{"state_name": "active+clean", "count": 281}, {"state_name": "active+undersized+degraded", "count": 8}], "recovering_bytes_per_sec": 0 }),
        );
        let r = classify(
            &s,
            &empty_tree(),
            &osd_df(&[41.2, 44.8, 39.5, 0.0, 47.1, 43.3]),
        );
        assert_eq!(
            r.state,
            ClusterHealthState::Degraded,
            "reasons: {:?}",
            r.reasons
        );
        assert!(!r.reasons.is_empty());
    }

    #[test]
    fn classifies_rebuilding_on_active_recovery() {
        let s = status(
            "HEALTH_WARN",
            json!({}),
            json!({ "num_osds": 6, "num_up_osds": 5, "num_in_osds": 6 }),
            json!({ "num_pgs": 289, "pgs_by_state": [{"state_name": "active+recovering", "count": 8}], "recovering_bytes_per_sec": 5_000_000 }),
        );
        let r = classify(
            &s,
            &empty_tree(),
            &osd_df(&[41.2, 44.8, 39.5, 0.0, 47.1, 43.3]),
        );
        assert_eq!(r.state, ClusterHealthState::Rebuilding);
    }

    #[test]
    fn classifies_at_risk_on_osd_utilization() {
        let s = status(
            "HEALTH_WARN",
            json!({}),
            json!({ "num_osds": 6, "num_up_osds": 6, "num_in_osds": 6 }),
            json!({ "num_pgs": 289, "pgs_by_state": [{"state_name": "active+clean", "count": 289}], "recovering_bytes_per_sec": 0 }),
        );
        let r = classify(
            &s,
            &empty_tree(),
            &osd_df(&[41.2, 44.8, 39.5, 88.0, 47.1, 43.3]),
        );
        assert_eq!(r.state, ClusterHealthState::AtRisk);
    }

    #[test]
    fn classifies_critical_on_health_err() {
        let s = status(
            "HEALTH_ERR",
            json!({ "OSD_FULL": { "severity": "HEALTH_ERR", "summary": { "message": "1 full osds" } } }),
            json!({ "num_osds": 6, "num_up_osds": 6, "num_in_osds": 6 }),
            json!({ "num_pgs": 289, "pgs_by_state": [{"state_name": "active+clean", "count": 289}], "recovering_bytes_per_sec": 0 }),
        );
        let r = classify(&s, &empty_tree(), &osd_df(&[41.2]));
        assert_eq!(r.state, ClusterHealthState::Critical);
    }

    #[test]
    fn critical_takes_precedence_over_zero_up_osds() {
        let s = status(
            "HEALTH_ERR",
            json!({}),
            json!({ "num_osds": 6, "num_up_osds": 0, "num_in_osds": 6 }),
            json!({ "num_pgs": 289, "pgs_by_state": [{"state_name": "down", "count": 289}], "recovering_bytes_per_sec": 0 }),
        );
        let r = classify(&s, &empty_tree(), &osd_df(&[]));
        assert_eq!(r.state, ClusterHealthState::Critical);
    }

    /// Precedence tie-break: an active recovery AND high OSD utilization together must classify
    /// as At Risk, not Rebuilding — rule order (At Risk checked before Rebuilding) matters.
    #[test]
    fn at_risk_takes_precedence_over_rebuilding() {
        let s = status(
            "HEALTH_WARN",
            json!({}),
            json!({ "num_osds": 6, "num_up_osds": 6, "num_in_osds": 6 }),
            json!({ "num_pgs": 289, "pgs_by_state": [{"state_name": "active+recovering", "count": 8}], "recovering_bytes_per_sec": 5_000_000 }),
        );
        let r = classify(
            &s,
            &empty_tree(),
            &osd_df(&[90.0, 44.8, 39.5, 41.0, 47.1, 43.3]),
        );
        assert_eq!(
            r.state,
            ClusterHealthState::AtRisk,
            "reasons: {:?}",
            r.reasons
        );
    }

    #[test]
    fn merge_rook_health_agrees_silently() {
        let s = status(
            "HEALTH_OK",
            json!({}),
            json!({ "num_osds": 6, "num_up_osds": 6, "num_in_osds": 6 }),
            json!({ "num_pgs": 100, "pgs_by_state": [{"state_name": "active+clean", "count": 100}], "recovering_bytes_per_sec": 0 }),
        );
        let mut r = classify(&s, &empty_tree(), &osd_df(&[10.0]));
        merge_rook_health(&mut r, Some("HEALTH_OK"));
        assert_eq!(r.sources, vec!["cli", "rook-crd"]);
        assert!(r.rook_disagreement.is_none());
    }

    #[test]
    fn merge_rook_health_flags_disagreement() {
        let s = status(
            "HEALTH_OK",
            json!({}),
            json!({ "num_osds": 6, "num_up_osds": 6, "num_in_osds": 6 }),
            json!({ "num_pgs": 100, "pgs_by_state": [{"state_name": "active+clean", "count": 100}], "recovering_bytes_per_sec": 0 }),
        );
        let mut r = classify(&s, &empty_tree(), &osd_df(&[10.0]));
        merge_rook_health(&mut r, Some("HEALTH_ERR"));
        assert!(r.rook_disagreement.is_some());
    }

    #[test]
    fn merge_rook_health_noop_when_absent() {
        let s = status("HEALTH_OK", json!({}), json!({}), json!({}));
        let mut r = classify(&s, &empty_tree(), &osd_df(&[]));
        merge_rook_health(&mut r, None);
        assert_eq!(r.sources, vec!["cli"]);
    }

    #[test]
    fn from_rook_only_maps_health_strings() {
        assert_eq!(
            from_rook_only("HEALTH_OK").state,
            ClusterHealthState::Healthy
        );
        assert_eq!(
            from_rook_only("HEALTH_WARN").state,
            ClusterHealthState::Degraded
        );
        assert_eq!(
            from_rook_only("HEALTH_ERR").state,
            ClusterHealthState::Critical
        );
        assert_eq!(from_rook_only("HEALTH_OK").sources, vec!["rook-crd"]);
    }
}
