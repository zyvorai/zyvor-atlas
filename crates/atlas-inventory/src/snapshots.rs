// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Snapshot records (PDF §11 storage_snapshots).

use anyhow::Result;
use atlas_api_types::StorageSnapshot;
use sqlx::{AnyPool, Row};

/// Insert a snapshot row.
#[allow(clippy::too_many_arguments)]
pub async fn insert_snapshot(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    volume_id: &str,
    name: &str,
    backend_native_id: Option<&str>,
    consistency: &str,
    state: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO storage_snapshots (id, tenant_id, volume_id, name, backend_native_id, consistency, state)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(volume_id)
    .bind(name)
    .bind(backend_native_id)
    .bind(consistency)
    .bind(state)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_state(pool: &AnyPool, id: &str, state: &str) -> Result<()> {
    sqlx::query("UPDATE storage_snapshots SET state=$1 WHERE id=$2")
        .bind(state)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record that the gateway retention-locked a snapshot until `until_unix` (unix seconds), in the
/// otherwise unused `retention_until` column (stored as a decimal string). Extend-only like the
/// lock itself: a smaller value than the stored one is ignored, atomically in SQL, so two
/// concurrent lock calls cannot lower it. Returns whether the stored value changed.
pub async fn set_locked_until(pool: &AnyPool, id: &str, until_unix: i64) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE storage_snapshots SET retention_until=$1 \
         WHERE id=$2 AND (retention_until IS NULL OR CAST(retention_until AS BIGINT) < $3)",
    )
    .bind(until_unix.to_string())
    .bind(id)
    .bind(until_unix)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Mark a snapshot protected (has dependent clones) or not.
pub async fn set_protected(pool: &AnyPool, id: &str, protected: bool) -> Result<()> {
    sqlx::query("UPDATE storage_snapshots SET protected=$1 WHERE id=$2")
        .bind(protected as i64)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_snapshot(pool: &AnyPool, id: &str) -> Result<Option<StorageSnapshot>> {
    let row = sqlx::query(select("WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_snapshot))
}

pub async fn list_snapshots(
    pool: &AnyPool,
    volume_id: Option<&str>,
) -> Result<Vec<StorageSnapshot>> {
    let rows = match volume_id {
        Some(v) => {
            sqlx::query(select("WHERE volume_id = $1 ORDER BY created_at DESC"))
                .bind(v)
                .fetch_all(pool)
                .await?
        }
        None => {
            sqlx::query(select("ORDER BY created_at DESC"))
                .fetch_all(pool)
                .await?
        }
    };
    Ok(rows.into_iter().map(row_to_snapshot).collect())
}

/// The oldest snapshot of `volume_id` with exactly this `name` (creation time, then id, so every
/// caller agrees which one is "first" when a race made several). Names are not unique in the
/// catalog; this backs the best-effort `if_not_exists` on snapshot create.
pub async fn find_by_name(
    pool: &AnyPool,
    volume_id: &str,
    name: &str,
) -> Result<Option<StorageSnapshot>> {
    let row = sqlx::query(select(
        "WHERE volume_id = $1 AND name = $2 ORDER BY created_at ASC, id ASC LIMIT 1",
    ))
    .bind(volume_id)
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_snapshot))
}

/// Snapshots in a given state — used to reconcile ones the create job's bounded bind-poll gave up
/// on (so they'd otherwise show "creating" forever even once the underlying VolumeSnapshot binds).
pub async fn list_by_state(pool: &AnyPool, state: &str) -> Result<Vec<StorageSnapshot>> {
    let rows = sqlx::query(select("WHERE state = $1 ORDER BY created_at DESC"))
        .bind(state)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row_to_snapshot).collect())
}

pub async fn delete_snapshot_row(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM storage_snapshots WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The most recent snapshot per volume, in one query — used by `protection::list_protection_status`
/// so a bank-scale fleet doesn't pay one query per volume.
pub async fn latest_by_volume(
    pool: &AnyPool,
) -> Result<std::collections::HashMap<String, StorageSnapshot>> {
    let rows = sqlx::query(
        "SELECT s.id, s.tenant_id, s.volume_id, s.name, s.backend_native_id, s.consistency, \
                s.state, s.protected, s.parent_snapshot_id, s.created_at, s.retention_until \
         FROM storage_snapshots s \
         INNER JOIN (SELECT volume_id, MAX(created_at) AS max_created FROM storage_snapshots GROUP BY volume_id) latest \
           ON latest.volume_id = s.volume_id AND latest.max_created = s.created_at",
    )
    .fetch_all(pool)
    .await?;
    let mut out = std::collections::HashMap::new();
    for r in rows {
        let snap = row_to_snapshot(r);
        out.entry(snap.volume_id.clone()).or_insert(snap);
    }
    Ok(out)
}

// `tail` is only ever a string literal from this module's own call sites (never user input — user
// data always goes through `$N` bind parameters), so asserting the assembled SQL safe is sound.
fn select(tail: &str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT id, tenant_id, volume_id, name, backend_native_id, consistency, state, protected, parent_snapshot_id, created_at, retention_until
         FROM storage_snapshots {tail}"
    ))
}

fn row_to_snapshot(r: sqlx::any::AnyRow) -> StorageSnapshot {
    StorageSnapshot {
        id: r.get("id"),
        tenant_id: r.get("tenant_id"),
        volume_id: r.get("volume_id"),
        name: r.get("name"),
        backend_native_id: r.get("backend_native_id"),
        consistency: r.get("consistency"),
        state: r.get("state"),
        protected: r.get::<i64, _>("protected") != 0,
        parent_snapshot_id: r.get("parent_snapshot_id"),
        created_at: r.get("created_at"),
        locked_until: r
            .get::<Option<String>, _>("retention_until")
            .and_then(|v| v.trim().parse::<i64>().ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_db() -> AnyPool {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let db = format!(
            "{}/atlas-snapshots-test-{}-{}.db",
            std::env::temp_dir().display(),
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        );
        let _ = std::fs::remove_file(&db);
        let url = format!("sqlite://{db}?mode=rwc");
        let pool = crate::connect(&url).await.unwrap();
        crate::migrate(&pool, &url).await.unwrap();
        sqlx::query(
            "INSERT INTO storage_backends (id, backend_type, mode, name, status)
             VALUES ('bkd1', 'native', 'managed', 'native', 'active')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO storage_volumes (id, tenant_id, backend_id, name, kind, size_bytes, state)
             VALUES ('vol1', 'global', 'bkd1', 'vol1', 'filesystem', 1000, 'ready')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn snap(pool: &AnyPool, id: &str, name: &str) {
        insert_snapshot(pool, id, "global", "vol1", name, None, "crash", "ready")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn locked_until_is_written_read_everywhere_and_only_raised() {
        let pool = temp_db().await;
        snap(&pool, "s1", "one").await;
        assert_eq!(get_snapshot(&pool, "s1").await.unwrap().unwrap().locked_until, None);

        assert!(set_locked_until(&pool, "s1", 2_000).await.unwrap());
        assert_eq!(get_snapshot(&pool, "s1").await.unwrap().unwrap().locked_until, Some(2_000));
        // A lower or equal value is ignored; a higher one replaces it. (Compared as numbers: 900 is
        // below 2000 although "900" sorts after "2000" as text.)
        assert!(!set_locked_until(&pool, "s1", 900).await.unwrap());
        assert!(!set_locked_until(&pool, "s1", 2_000).await.unwrap());
        assert_eq!(get_snapshot(&pool, "s1").await.unwrap().unwrap().locked_until, Some(2_000));
        assert!(set_locked_until(&pool, "s1", 10_000).await.unwrap());
        assert!(!set_locked_until(&pool, "s1", 9_999).await.unwrap());

        // Every reader that builds a StorageSnapshot returns it.
        let listed = list_snapshots(&pool, Some("vol1")).await.unwrap();
        assert_eq!(listed[0].locked_until, Some(10_000));
        let latest = latest_by_volume(&pool).await.unwrap();
        assert_eq!(latest["vol1"].locked_until, Some(10_000));
        assert_eq!(find_by_name(&pool, "vol1", "one").await.unwrap().unwrap().locked_until, Some(10_000));
        assert_eq!(list_by_state(&pool, "ready").await.unwrap()[0].locked_until, Some(10_000));

        // An unlocked snapshot omits the field in JSON; a locked one carries a number.
        snap(&pool, "s2", "two").await;
        let s2 = get_snapshot(&pool, "s2").await.unwrap().unwrap();
        assert!(serde_json::to_value(&s2).unwrap().get("locked_until").is_none());
        let s1 = get_snapshot(&pool, "s1").await.unwrap().unwrap();
        assert_eq!(serde_json::to_value(&s1).unwrap()["locked_until"], 10_000);
    }

    #[tokio::test]
    async fn find_by_name_returns_the_oldest_same_named_snapshot() {
        let pool = temp_db().await;
        snap(&pool, "b", "dup").await;
        snap(&pool, "a", "dup").await;
        snap(&pool, "c", "other").await;
        // Same creation instant or not, the choice is deterministic (created_at, then id).
        let first = find_by_name(&pool, "vol1", "dup").await.unwrap().unwrap();
        let again = find_by_name(&pool, "vol1", "dup").await.unwrap().unwrap();
        assert_eq!(first.id, again.id);
        assert!(first.id == "a" || first.id == "b");
        assert!(find_by_name(&pool, "vol1", "missing").await.unwrap().is_none());
        assert!(find_by_name(&pool, "vol2", "dup").await.unwrap().is_none());
    }
}
