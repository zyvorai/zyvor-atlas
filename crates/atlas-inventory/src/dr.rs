// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Cross-cluster DR state: mirroring peers + per-image mirror records (day-2 scaffolding). The real
//! `rbd mirror` operations run as jobs; this is the control-plane catalog they update.

use anyhow::Result;
use sqlx::{AnyPool, Row};

use crate::now_rfc3339;

// ---- peers ----

/// Register (or update) a mirroring peer cluster. `secret_ref` names a k8s Secret with its bootstrap
/// token — never the token itself.
pub async fn register_peer(
    pool: &AnyPool,
    id: &str,
    name: &str,
    cluster_fsid: Option<&str>,
    direction: &str,
    secret_ref: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO dr_peers (id, name, cluster_fsid, direction, bootstrap_secret_ref, state)
         VALUES ($1, $2, $3, $4, $5, 'registered')
         ON CONFLICT(id) DO UPDATE SET name=excluded.name, cluster_fsid=excluded.cluster_fsid,
            direction=excluded.direction, bootstrap_secret_ref=excluded.bootstrap_secret_ref",
    )
    .bind(id)
    .bind(name)
    .bind(cluster_fsid)
    .bind(direction)
    .bind(secret_ref)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove a mirroring peer and any mirrors that referenced it (stale/decommissioned peer).
pub async fn delete_peer(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM dr_mirrors WHERE peer_id=$1")
        .bind(id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM dr_peers WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_peers(pool: &AnyPool) -> Result<Vec<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT id, name, cluster_fsid, direction, bootstrap_secret_ref, state, created_at
         FROM dr_peers ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.get::<String, _>("id"),
                "name": r.get::<String, _>("name"),
                "cluster_fsid": r.get::<Option<String>, _>("cluster_fsid"),
                "direction": r.get::<String, _>("direction"),
                "bootstrap_secret_ref": r.get::<Option<String>, _>("bootstrap_secret_ref"),
                "state": r.get::<String, _>("state"),
            })
        })
        .collect())
}

// ---- pool mirroring mode ----

/// Record the mirroring mode Atlas applied to a pool (`image` or `pool`).
pub async fn set_pool_mode(pool: &AnyPool, rbd_pool: &str, mode: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO dr_pool_mirroring (pool, mode, updated_at) VALUES ($1, $2, $3)
         ON CONFLICT(pool) DO UPDATE SET mode=excluded.mode, updated_at=excluded.updated_at",
    )
    .bind(rbd_pool)
    .bind(mode)
    .bind(now_rfc3339(chrono::Utc::now()))
    .execute(pool)
    .await?;
    Ok(())
}

/// The mode last recorded for a pool, if any.
pub async fn pool_mode(pool: &AnyPool, rbd_pool: &str) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT mode FROM dr_pool_mirroring WHERE pool=$1")
            .bind(rbd_pool)
            .fetch_optional(pool)
            .await?,
    )
}

// ---- mirrors ----

/// Record (or update) a mirrored image. Keyed uniquely by `(pool, image)`.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_mirror(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    volume_id: Option<&str>,
    rbd_pool: &str,
    image: &str,
    peer_id: Option<&str>,
    mode: &str,
    role: &str,
    state: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO dr_mirrors (id, tenant_id, volume_id, pool, image, peer_id, mode, role, state)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT(pool, image) DO UPDATE SET peer_id=excluded.peer_id, mode=excluded.mode,
            role=excluded.role, state=excluded.state,
            updated_at=$10",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(volume_id)
    .bind(rbd_pool)
    .bind(image)
    .bind(peer_id)
    .bind(mode)
    .bind(role)
    .bind(state)
    .bind(now_rfc3339(chrono::Utc::now()))
    .execute(pool)
    .await?;
    Ok(())
}

/// Update a mirror's role and/or state. Returns whether a row changed.
pub async fn set_mirror(pool: &AnyPool, id: &str, role: &str, state: &str) -> Result<bool> {
    let res = sqlx::query("UPDATE dr_mirrors SET role=$1, state=$2, updated_at=$3 WHERE id=$4")
        .bind(role)
        .bind(state)
        .bind(now_rfc3339(chrono::Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Update only `state`, leaving `role` untouched. Used by the enable job so it cannot race a
/// concurrent demote/promote that already moved the role.
pub async fn set_mirror_state(pool: &AnyPool, id: &str, state: &str) -> Result<bool> {
    let res = sqlx::query("UPDATE dr_mirrors SET state=$1, updated_at=$2 WHERE id=$3")
        .bind(state)
        .bind(now_rfc3339(chrono::Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// A mirror's `(pool, image, role)` — used by the promote/demote/disable jobs.
pub async fn mirror_target(pool: &AnyPool, id: &str) -> Result<Option<(String, String, String)>> {
    let row = sqlx::query("SELECT pool, image, role FROM dr_mirrors WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| (r.get("pool"), r.get("image"), r.get("role"))))
}

pub async fn list_mirrors(pool: &AnyPool) -> Result<Vec<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT id, tenant_id, volume_id, pool, image, peer_id, mode, role, state, rpo_seconds,
                last_failover_at, last_error, force_promoted, updated_at
         FROM dr_mirrors ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.get::<String, _>("id"),
                "tenant_id": r.get::<String, _>("tenant_id"),
                "volume_id": r.get::<Option<String>, _>("volume_id"),
                "pool": r.get::<String, _>("pool"),
                "image": r.get::<String, _>("image"),
                "peer_id": r.get::<Option<String>, _>("peer_id"),
                "mode": r.get::<String, _>("mode"),
                "role": r.get::<String, _>("role"),
                "state": r.get::<String, _>("state"),
                "rpo_seconds": r.get::<Option<i64>, _>("rpo_seconds"),
                "last_failover_at": r.get::<Option<String>, _>("last_failover_at"),
                "last_error": r.get::<Option<String>, _>("last_error"),
                "force_promoted": r.get::<i64, _>("force_promoted") != 0,
                "updated_at": r.get::<String, _>("updated_at"),
            })
        })
        .collect())
}

/// Whether a peer id exists in the catalog.
pub async fn peer_exists(pool: &AnyPool, id: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dr_peers WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(n > 0)
}

/// Full mirror row for transition guards: `(pool, image, role, state)`.
pub async fn mirror_detail(
    pool: &AnyPool,
    id: &str,
) -> Result<Option<(String, String, String, String)>> {
    let row = sqlx::query("SELECT pool, image, role, state FROM dr_mirrors WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| (r.get("pool"), r.get("image"), r.get("role"), r.get("state"))))
}

/// Stamp an error onto a mirror (real `rbd mirror` failure) without changing role.
pub async fn set_mirror_error(pool: &AnyPool, id: &str, error: &str) -> Result<()> {
    sqlx::query(
        "UPDATE dr_mirrors SET state='error', last_error=$1,
         updated_at=$2 WHERE id=$3",
    )
    .bind(error)
    .bind(now_rfc3339(chrono::Utc::now()))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a successful promote/failover drill.
pub async fn record_failover(pool: &AnyPool, id: &str, force: bool) -> Result<()> {
    sqlx::query(
        "UPDATE dr_mirrors SET last_failover_at=$1,
         last_error=NULL, force_promoted=$2,
         updated_at=$3 WHERE id=$4",
    )
    .bind(now_rfc3339(chrono::Utc::now()))
    .bind(if force { 1 } else { 0 })
    .bind(now_rfc3339(chrono::Utc::now()))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Set observed RPO seconds for a mirrored image (operator/refresh path).
pub async fn set_rpo(pool: &AnyPool, id: &str, rpo_seconds: Option<i64>) -> Result<bool> {
    let res = sqlx::query("UPDATE dr_mirrors SET rpo_seconds=$1, updated_at=$2 WHERE id=$3")
        .bind(rpo_seconds)
        .bind(now_rfc3339(chrono::Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Preflight checklist for a DR failover drill (control-plane view; does not talk to Ceph).
///
/// `ready` means the **catalog** is coherent enough to run a failover *job*. `dataplane_verified`
/// is passed in from `Config::dr_dataplane_verified` (see its doc comment) — `false` unless this
/// specific deployment's operator has personally completed the live two-site drill in
/// `docs/DR.md`; never inferred from catalog state alone.
pub async fn preflight(pool: &AnyPool, dataplane_verified: bool) -> Result<serde_json::Value> {
    let peers = list_peers(pool).await?;
    let mirrors = list_mirrors(pool).await?;
    let mut checks = Vec::new();
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    let peer_ok = !peers.is_empty();
    checks.push(serde_json::json!({
        "id": "peer_registered", "ok": peer_ok,
        "detail": if peer_ok { format!("{} peer(s)", peers.len()) } else { "no DR peers registered".into() }
    }));
    if !peer_ok {
        blockers.push("register at least one peer via POST /dr/peers");
    }

    let with_secret = peers
        .iter()
        .filter(|p| {
            p["bootstrap_secret_ref"]
                .as_str()
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        })
        .count();
    let secret_ok = !peer_ok || with_secret == peers.len();
    checks.push(serde_json::json!({
        "id": "peer_secret_refs", "ok": secret_ok,
        "detail": format!("{with_secret}/{} peers have bootstrap_secret_ref", peers.len())
    }));
    if !secret_ok {
        blockers.push("every peer should reference a k8s Secret holding the bootstrap token");
    }

    let errored: Vec<_> = mirrors
        .iter()
        .filter(|m| m["state"] == "error")
        .map(|m| m["id"].as_str().unwrap_or("").to_string())
        .collect();
    let err_ok = errored.is_empty();
    checks.push(serde_json::json!({
        "id": "no_mirror_errors", "ok": err_ok,
        "detail": if err_ok { "no mirrors in error state".into() } else { format!("error: {}", errored.join(",")) }
    }));
    if !err_ok {
        blockers.push("resolve mirrors in state=error before failing over");
    }

    let enabled = mirrors.iter().filter(|m| m["state"] == "enabled").count();
    let enabled_ok = mirrors.is_empty() || enabled > 0;
    checks.push(serde_json::json!({
        "id": "enabled_mirrors", "ok": enabled_ok,
        "detail": format!("{enabled}/{} mirror(s) in state=enabled", mirrors.len())
    }));
    if !enabled_ok {
        blockers.push("no enabled mirrors — enable mirroring on a volume before failover");
    }

    let secondaries = mirrors.iter().filter(|m| m["role"] == "secondary").count();
    let failover_ready = secondaries > 0;
    checks.push(serde_json::json!({
        "id": "secondary_available", "ok": failover_ready || mirrors.is_empty(),
        "detail": format!("{secondaries} secondary mirror(s) can be promoted")
    }));
    if !failover_ready && !mirrors.is_empty() {
        warnings.push(
            "no secondary role yet — demote a primary (or wait for peer sync) before promote",
        );
    }

    let with_rpo = mirrors
        .iter()
        .filter(|m| m["rpo_seconds"].as_i64().is_some())
        .count();
    let rpo_ok = mirrors.is_empty() || with_rpo > 0;
    checks.push(serde_json::json!({
        "id": "rpo_observed", "ok": rpo_ok,
        "detail": if mirrors.is_empty() {
            "no mirrors yet".into()
        } else {
            format!("{with_rpo}/{} mirror(s) have an observed RPO", mirrors.len())
        }
    }));
    if !rpo_ok {
        warnings.push("record observed RPO via POST /dr/mirrors/{id}/rpo after a live sync sample");
    }

    checks.push(serde_json::json!({
        "id": "dataplane_verified", "ok": dataplane_verified,
        "detail": if dataplane_verified {
            "this deployment has completed the live two-site rbd mirror drill — see docs/DR.md"
        } else {
            "live two-site rbd mirror verification pending — see docs/DR.md"
        }
    }));
    if !dataplane_verified {
        warnings.push("dataplane unverified: treat failover as a control-plane drill until a peer cluster exists");
    }

    Ok(serde_json::json!({
        "ready": blockers.is_empty(),
        "control_plane_ready": blockers.is_empty(),
        "dataplane_verified": dataplane_verified,
        "checks": checks,
        "blockers": blockers,
        "warnings": warnings,
        "peers": peers.len(),
        "mirrors": mirrors.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    async fn mem_pool() -> AnyPool {
        let db = format!(
            "{}/atlas-dr-{}-{}.db",
            std::env::temp_dir().display(),
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst),
        );
        let _ = std::fs::remove_file(&db);
        let url = format!("sqlite://{db}?mode=rwc");
        let pool = crate::connect(&url).await.unwrap();
        crate::migrate(&pool, &url).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn preflight_blocks_without_peers_and_stays_dataplane_unverified() {
        let pool = mem_pool().await;
        let pre = preflight(&pool, false).await.unwrap();
        assert_eq!(pre["ready"], false);
        assert_eq!(pre["dataplane_verified"], false);
        assert!(pre["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b.as_str().unwrap().contains("peer")));

        register_peer(
            &pool,
            "p1",
            "dc2",
            Some("fsid"),
            "bidirectional",
            Some("sec"),
        )
        .await
        .unwrap();
        let pre2 = preflight(&pool, false).await.unwrap();
        assert_eq!(pre2["ready"], true);
        assert_eq!(pre2["control_plane_ready"], true);
        assert_eq!(pre2["dataplane_verified"], false);
        assert!(pre2["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| { w.as_str().unwrap().contains("dataplane unverified") }));
    }

    #[tokio::test]
    async fn preflight_reports_dataplane_verified_when_configured() {
        // Mirrors Config::dr_dataplane_verified's contract: this flag comes from the caller
        // (an operator who has personally completed the live two-site drill), never inferred
        // from catalog state — so passing true must propagate even with an empty catalog.
        let pool = mem_pool().await;
        let pre = preflight(&pool, true).await.unwrap();
        assert_eq!(pre["dataplane_verified"], true);
        assert!(
            !pre["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| { w.as_str().unwrap().contains("dataplane unverified") }),
            "the unverified-dataplane warning must not fire once verified"
        );
    }
}
