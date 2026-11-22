// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Per-volume "Protection Status" — synthesizes replication factor, last snapshot/backup, DR
//! mirror state, and RPO/RTO target-vs-actual into one verdict, so an operator doesn't have to
//! cross-reference the snapshots/backups/dr/schedules views by hand to answer "is this volume
//! actually protected?". Every input already exists elsewhere in this crate; this module reads,
//! it does not write.
//!
//! RTO is deliberately **not estimated** here (no size÷throughput heuristic) — there is no
//! historical restore-duration data wired up yet (`JobRecord` doesn't expose the jobs table's
//! `started_at`/`completed_at` columns). `rto_note` always says so plainly rather than presenting
//! a number a bank might mistake for a measured guarantee.

use std::collections::HashMap;

use anyhow::Result;
use atlas_api_types::{BackupRecord, ClusterHealthState, SnapshotSchedule, StorageSnapshot};
use chrono::Utc;
use sqlx::{AnyPool, Row};

use crate::{backups, buckets, dr, schedules, snapshots};

/// A recovery point older than this many multiples of the RPO target is treated as At Risk.
const AT_RISK_RPO_MULTIPLE: i64 = 4;

#[derive(Debug, Clone, serde::Serialize)]
pub struct VolumeProtectionStatus {
    pub volume_id: String,
    pub volume_name: String,
    /// "Ceph RBD" / "CephFS" / "Ceph RGW", derived from the volume's kind.
    pub storage_backend: String,
    pub replication_factor: Option<i64>,
    pub last_snapshot_at: Option<String>,
    pub last_backup_at: Option<String>,
    pub last_backup_location: Option<String>,
    pub dr_role: Option<String>,
    pub dr_state: Option<String>,
    pub rpo_target_seconds: Option<i64>,
    /// "policy" | "schedule" | "none".
    pub rpo_target_source: String,
    pub rpo_current_seconds: Option<i64>,
    /// Policy-sourced only; always `None` until a policy-CRUD feature populates `storage_policies`.
    pub rto_target_seconds: Option<i64>,
    pub rto_note: String,
    pub verdict: ClusterHealthState,
    pub verdict_reasons: Vec<String>,
}

/// Per-volume RPO/RTO targets from `storage_policies.protection`, keyed by volume id. Defensive
/// by design: nothing in the codebase writes to `storage_policies` yet (the in-code policy catalog
/// in `atlas-policy` is not table-backed), so this map is expected to be empty in practice today —
/// see `deploy/postgres-lab/README.md`-style honesty notes elsewhere in this session. Forward-
/// compatible at zero cost for whenever a policy-CRUD feature starts populating the table.
async fn policy_targets_by_volume(
    pool: &AnyPool,
) -> Result<HashMap<String, (Option<i64>, Option<i64>)>> {
    // `protection` (raw JSON text) is pulled back and parsed in Rust rather than extracted via
    // SQLite's json_extract() server-side — no portable equivalent exists whose query text is
    // identical on Postgres (see atlas_inventory::set_volume_qos's comment for the same tradeoff).
    let rows = sqlx::query(
        "SELECT v.id AS volume_id, p.protection AS protection
         FROM storage_volumes v
         LEFT JOIN storage_policies p ON p.id = v.policy_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let protection: Option<String> = r.get("protection");
            let parsed: Option<serde_json::Value> = protection
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            let rpo = parsed
                .as_ref()
                .and_then(|v| v.get("rpo_target_seconds"))
                .and_then(|v| v.as_i64());
            let rto = parsed
                .as_ref()
                .and_then(|v| v.get("rto_target_seconds"))
                .and_then(|v| v.as_i64());
            (r.get::<String, _>("volume_id"), (rpo, rto))
        })
        .collect())
}

fn parse_ts(ts: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn storage_backend_label(kind: atlas_api_types::VolumeKind) -> &'static str {
    match kind {
        atlas_api_types::VolumeKind::Block => "Ceph RBD",
        atlas_api_types::VolumeKind::Filesystem => "CephFS",
        atlas_api_types::VolumeKind::Object => "Ceph RGW",
    }
}

/// Everything needed to compute a fleet's worth of protection statuses, fetched once (not
/// per-volume) so a bank-scale deployment doesn't pay N+1 queries per volume.
struct Context {
    pools: HashMap<String, atlas_api_types::StoragePool>,
    latest_snapshot: HashMap<String, StorageSnapshot>,
    latest_backup: HashMap<String, BackupRecord>,
    buckets_by_id: HashMap<String, atlas_api_types::StorageBucket>,
    mirrors_by_volume: HashMap<String, serde_json::Value>,
    schedules_by_volume: HashMap<String, Vec<SnapshotSchedule>>,
    policy_targets: HashMap<String, (Option<i64>, Option<i64>)>,
}

async fn build_context(pool: &AnyPool) -> Result<Context> {
    let pools = crate::list_pools(pool)
        .await?
        .into_iter()
        .map(|p| (p.id.clone(), p))
        .collect();
    let latest_snapshot = snapshots::latest_by_volume(pool).await?;
    let latest_backup = backups::latest_by_volume(pool).await?;
    let buckets_by_id = buckets::list_buckets(pool)
        .await?
        .into_iter()
        .map(|b| (b.id.clone(), b))
        .collect();
    let mut mirrors_by_volume = HashMap::new();
    for m in dr::list_mirrors(pool).await? {
        if let Some(vid) = m["volume_id"].as_str() {
            mirrors_by_volume.insert(vid.to_string(), m);
        }
    }
    let mut schedules_by_volume: HashMap<String, Vec<SnapshotSchedule>> = HashMap::new();
    for s in schedules::list(pool, None).await? {
        schedules_by_volume
            .entry(s.volume_id.clone())
            .or_default()
            .push(s);
    }
    let policy_targets = policy_targets_by_volume(pool).await?;
    Ok(Context {
        pools,
        latest_snapshot,
        latest_backup,
        buckets_by_id,
        mirrors_by_volume,
        schedules_by_volume,
        policy_targets,
    })
}

fn compute(v: &atlas_api_types::StorageVolume, ctx: &Context) -> VolumeProtectionStatus {
    let replication_factor = v
        .pool_id
        .as_ref()
        .and_then(|pid| ctx.pools.get(pid))
        .and_then(|p| p.replica_size);
    let last_snapshot = ctx.latest_snapshot.get(&v.id);
    let last_backup = ctx.latest_backup.get(&v.id);
    let last_backup_location = last_backup.and_then(|b| {
        ctx.buckets_by_id.get(&b.bucket_id).map(|bucket| {
            let name = bucket.bucket_name.as_deref().unwrap_or(&bucket.name);
            match &bucket.endpoint {
                Some(ep) => format!("{name} ({ep})"),
                None => name.to_string(),
            }
        })
    });

    let mirror = ctx.mirrors_by_volume.get(&v.id);
    let dr_role = mirror.and_then(|m| m["role"].as_str()).map(str::to_string);
    let dr_state = mirror.and_then(|m| m["state"].as_str()).map(str::to_string);
    let mirror_rpo_observed = mirror.and_then(|m| m["rpo_seconds"].as_i64());
    let mirror_errored = dr_state.as_deref() == Some("error");

    let (policy_rpo, policy_rto) = ctx
        .policy_targets
        .get(&v.id)
        .copied()
        .unwrap_or((None, None));
    let schedules = ctx.schedules_by_volume.get(&v.id);
    let schedule_rpo = schedules.and_then(|ss| {
        ss.iter()
            .filter(|s| s.enabled)
            .map(|s| s.interval_secs)
            .min()
    });
    let (rpo_target_seconds, rpo_target_source) = match (policy_rpo, schedule_rpo) {
        (Some(p), _) => (Some(p), "policy"),
        (None, Some(s)) => (Some(s), "schedule"),
        (None, None) => (None, "none"),
    };

    let last_protection_point = [
        last_snapshot.and_then(|s| s.created_at.as_deref()),
        last_backup.and_then(|b| b.created_at.as_deref()),
    ]
    .into_iter()
    .flatten()
    .filter_map(parse_ts)
    .max();
    let protection_point_age_seconds =
        last_protection_point.map(|dt| (Utc::now() - dt).num_seconds().max(0));

    let rpo_current_seconds = mirror_rpo_observed.or(protection_point_age_seconds);

    // A schedule that's enabled, past due, and has never run — likely broken (misconfigured
    // target, permission error the job silently swallowed, etc.).
    let never_fired_overdue_schedule = schedules.is_some_and(|ss| {
        ss.iter().any(|s| {
            s.enabled
                && s.last_run_at.is_none()
                && parse_ts(&s.next_run_at).is_some_and(|t| t < Utc::now())
        })
    });

    let mut reasons: Vec<String> = Vec::new();
    let verdict = if last_snapshot.is_none() && last_backup.is_none() {
        reasons.push("no snapshot or backup has ever been recorded for this volume".into());
        ClusterHealthState::Critical
    } else if mirror_errored {
        reasons.push(format!(
            "DR mirror is in error state{}",
            mirror
                .and_then(|m| m["last_error"].as_str())
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        ));
        ClusterHealthState::Critical
    } else if rpo_target_seconds
        .is_some_and(|t| protection_point_age_seconds.is_some_and(|a| a > t * AT_RISK_RPO_MULTIPLE))
        || (replication_factor == Some(1) && mirror.is_none())
        || never_fired_overdue_schedule
    {
        if let (Some(t), Some(a)) = (rpo_target_seconds, protection_point_age_seconds) {
            if a > t * AT_RISK_RPO_MULTIPLE {
                reasons.push(format!("last recovery point is {a}s old, over {AT_RISK_RPO_MULTIPLE}x the {t}s RPO target"));
            }
        }
        if replication_factor == Some(1) && mirror.is_none() {
            reasons.push("single replica with no DR mirror configured".into());
        }
        if never_fired_overdue_schedule {
            reasons.push("an enabled schedule is overdue and has never run".into());
        }
        ClusterHealthState::AtRisk
    } else if matches!(
        dr_state.as_deref(),
        Some("enabling")
            | Some("disabling")
            | Some("promoting")
            | Some("demoting")
            | Some("resyncing")
    ) {
        reasons.push(format!(
            "DR mirror transition in progress ({})",
            dr_state.as_deref().unwrap_or("")
        ));
        ClusterHealthState::Rebuilding
    } else if rpo_target_seconds.is_none() {
        reasons.push(
            "no RPO target is knowable (no schedule, no policy) — freshness can't be graded".into(),
        );
        ClusterHealthState::Degraded
    } else if rpo_target_seconds
        .is_some_and(|t| protection_point_age_seconds.is_some_and(|a| a > t))
    {
        reasons.push("last recovery point is older than the RPO target".into());
        ClusterHealthState::Degraded
    } else {
        reasons.push("recovery point within target, no mirror errors".into());
        ClusterHealthState::Healthy
    };

    VolumeProtectionStatus {
        volume_id: v.id.clone(),
        volume_name: v.name.clone(),
        storage_backend: storage_backend_label(v.kind).to_string(),
        replication_factor,
        last_snapshot_at: last_snapshot.and_then(|s| s.created_at.clone()),
        last_backup_at: last_backup.and_then(|b| b.created_at.clone()),
        last_backup_location,
        dr_role,
        dr_state,
        rpo_target_seconds,
        rpo_target_source: rpo_target_source.to_string(),
        rpo_current_seconds,
        rto_target_seconds: policy_rto,
        rto_note: "not measured — no restore drill on record".to_string(),
        verdict,
        verdict_reasons: reasons,
    }
}

pub async fn volume_protection_status(
    pool: &AnyPool,
    volume_id: &str,
) -> Result<Option<VolumeProtectionStatus>> {
    let Some(v) = crate::get_volume(pool, volume_id).await? else {
        return Ok(None);
    };
    let ctx = build_context(pool).await?;
    Ok(Some(compute(&v, &ctx)))
}

pub async fn list_protection_status(
    pool: &AnyPool,
    tenant_id: Option<&str>,
) -> Result<Vec<VolumeProtectionStatus>> {
    let volumes = crate::list_volumes_filtered(pool, None, tenant_id, None, None).await?;
    let ctx = build_context(pool).await?;
    Ok(volumes.iter().map(|v| compute(v, &ctx)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_db() -> AnyPool {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let db = format!(
            "{}/atlas-protection-test-{}-{}.db",
            std::env::temp_dir().display(),
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        );
        let _ = std::fs::remove_file(&db);
        let url = format!("sqlite://{db}?mode=rwc");
        let pool = crate::connect(&url).await.unwrap();
        crate::migrate(&pool, &url).await.unwrap();
        pool
    }

    /// FK prerequisites for storage_volumes/storage_pools: a backend row and a cluster row.
    /// Idempotent (`ON CONFLICT DO NOTHING`) since multiple seed_volume/seed_pool calls in one
    /// test share the same backend/cluster.
    async fn seed_backend_and_cluster(pool: &AnyPool) {
        sqlx::query(
            "INSERT INTO storage_backends (id, backend_type, mode, name, status)
             VALUES ('bkd1', 'ceph', 'managed_rook', 'ceph', 'active')
             ON CONFLICT DO NOTHING",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO storage_clusters (id, backend_id, name) VALUES ('c1', 'bkd1', 'ceph')
             ON CONFLICT DO NOTHING",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_volume(pool: &AnyPool, id: &str, pool_id: Option<&str>) {
        seed_backend_and_cluster(pool).await;
        sqlx::query(
            "INSERT INTO storage_volumes (id, tenant_id, backend_id, name, kind, size_bytes, state, pool_id)
             VALUES ($1, 'global', 'bkd1', $2, 'block', 1000, 'ready', $3)",
        )
        .bind(id)
        .bind(id)
        .bind(pool_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_pool(pool: &AnyPool, id: &str, replica_size: i64) {
        seed_backend_and_cluster(pool).await;
        sqlx::query(
            "INSERT INTO storage_pools (id, cluster_id, name, kind, replica_size)
             VALUES ($1, 'c1', $2, 'rbd', $3)",
        )
        .bind(id)
        .bind(id)
        .bind(replica_size)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_snapshot(pool: &AnyPool, volume_id: &str, created_at: &str) {
        sqlx::query(
            "INSERT INTO storage_snapshots (id, tenant_id, volume_id, name, consistency, state, created_at)
             VALUES ($1, 'global', $2, 'snap', 'crash', 'ready', $3)",
        )
        .bind(format!("snap-{volume_id}-{created_at}"))
        .bind(volume_id)
        .bind(created_at)
        .execute(pool)
        .await
        .unwrap();
    }

    fn now_minus(secs: i64) -> String {
        (Utc::now() - chrono::Duration::seconds(secs))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    #[tokio::test]
    async fn no_protection_is_critical() {
        let pool = temp_db().await;
        seed_volume(&pool, "v1", None).await;
        let status = volume_protection_status(&pool, "v1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.verdict,
            ClusterHealthState::Critical,
            "{:?}",
            status.verdict_reasons
        );
    }

    #[tokio::test]
    async fn mirror_error_is_critical() {
        let pool = temp_db().await;
        seed_volume(&pool, "v1", None).await;
        seed_snapshot(&pool, "v1", &now_minus(60)).await;
        dr::upsert_mirror(
            &pool,
            "m1",
            "global",
            Some("v1"),
            "rbd",
            "v1",
            None,
            "snapshot",
            "primary",
            "error",
        )
        .await
        .unwrap();
        let status = volume_protection_status(&pool, "v1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.verdict,
            ClusterHealthState::Critical,
            "{:?}",
            status.verdict_reasons
        );
    }

    #[tokio::test]
    async fn stale_beyond_4x_target_is_at_risk() {
        let pool = temp_db().await;
        seed_volume(&pool, "v1", None).await;
        schedules::insert(
            &pool, "sch1", "global", "v1", "snapshot", None, "manifest", 60, 5,
        )
        .await
        .unwrap();
        seed_snapshot(&pool, "v1", &now_minus(1000)).await; // 1000s old, target 60s -> >4x
        let status = volume_protection_status(&pool, "v1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.verdict,
            ClusterHealthState::AtRisk,
            "{:?}",
            status.verdict_reasons
        );
    }

    #[tokio::test]
    async fn single_replica_no_mirror_is_at_risk() {
        let pool = temp_db().await;
        seed_pool(&pool, "p1", 1).await;
        seed_volume(&pool, "v1", Some("p1")).await;
        seed_snapshot(&pool, "v1", &now_minus(10)).await;
        let status = volume_protection_status(&pool, "v1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.verdict,
            ClusterHealthState::AtRisk,
            "{:?}",
            status.verdict_reasons
        );
        assert_eq!(status.replication_factor, Some(1));
    }

    #[tokio::test]
    async fn mirror_transition_is_rebuilding() {
        let pool = temp_db().await;
        seed_pool(&pool, "p1", 3).await;
        seed_volume(&pool, "v1", Some("p1")).await;
        seed_snapshot(&pool, "v1", &now_minus(10)).await;
        dr::upsert_mirror(
            &pool,
            "m1",
            "global",
            Some("v1"),
            "rbd",
            "v1",
            None,
            "snapshot",
            "primary",
            "enabling",
        )
        .await
        .unwrap();
        let status = volume_protection_status(&pool, "v1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.verdict,
            ClusterHealthState::Rebuilding,
            "{:?}",
            status.verdict_reasons
        );
    }

    #[tokio::test]
    async fn mild_lag_within_4x_is_degraded() {
        let pool = temp_db().await;
        seed_pool(&pool, "p1", 3).await;
        seed_volume(&pool, "v1", Some("p1")).await;
        schedules::insert(
            &pool, "sch1", "global", "v1", "snapshot", None, "manifest", 60, 5,
        )
        .await
        .unwrap();
        seed_snapshot(&pool, "v1", &now_minus(120)).await; // 2x target, within 4x
        let status = volume_protection_status(&pool, "v1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.verdict,
            ClusterHealthState::Degraded,
            "{:?}",
            status.verdict_reasons
        );
    }

    #[tokio::test]
    async fn fresh_and_replicated_is_healthy() {
        let pool = temp_db().await;
        seed_pool(&pool, "p1", 3).await;
        seed_volume(&pool, "v1", Some("p1")).await;
        schedules::insert(
            &pool, "sch1", "global", "v1", "snapshot", None, "manifest", 3600, 5,
        )
        .await
        .unwrap();
        seed_snapshot(&pool, "v1", &now_minus(10)).await;
        dr::upsert_mirror(
            &pool,
            "m1",
            "global",
            Some("v1"),
            "rbd",
            "v1",
            None,
            "snapshot",
            "primary",
            "enabled",
        )
        .await
        .unwrap();
        let status = volume_protection_status(&pool, "v1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.verdict,
            ClusterHealthState::Healthy,
            "{:?}",
            status.verdict_reasons
        );
        assert_eq!(status.rto_note, "not measured — no restore drill on record");
        assert!(status.rto_target_seconds.is_none());
    }

    #[tokio::test]
    async fn no_target_knowable_caps_at_degraded_not_healthy() {
        let pool = temp_db().await;
        seed_volume(&pool, "v1", None).await;
        seed_snapshot(&pool, "v1", &now_minus(10)).await;
        let status = volume_protection_status(&pool, "v1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.verdict,
            ClusterHealthState::Degraded,
            "{:?}",
            status.verdict_reasons
        );
        assert_eq!(status.rpo_target_source, "none");
    }
}
