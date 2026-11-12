// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Cross-replica rate-limit sync (day-2 HA governance). Backs
//! `crates/atlas-gateway/src/state.rs::RateLimiter`'s periodic background sync — see
//! `migrations/0034_rate_limit_counters.sql`'s doc comment for the full design: the hot,
//! per-request `allow()` check stays fully in-process and synchronous (so it's safe to call from
//! the gRPC path's synchronous `tonic::Interceptor`); only this periodic, async, best-effort sync
//! ever touches the database.

use anyhow::Result;
use sqlx::{AnyPool, Row};

/// Overwrite this replica's own current-window count (not add to it — each sync tick reports the
/// replica's absolute count for that window, so a retried/duplicate sync can't double-count).
pub async fn upsert_replica_count(
    pool: &AnyPool,
    actor_id: &str,
    window_minute: i64,
    replica_id: &str,
    count: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO rate_limit_counters (actor_id, window_minute, replica_id, count)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT(actor_id, window_minute, replica_id) DO UPDATE SET count=excluded.count",
    )
    .bind(actor_id)
    .bind(window_minute)
    .bind(replica_id)
    .bind(count)
    .execute(pool)
    .await?;
    Ok(())
}

/// Summed count per actor across every replica for one window — the cluster-wide total `allow()`
/// ultimately enforces against.
pub async fn cluster_totals(pool: &AnyPool, window_minute: i64) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query(
        "SELECT actor_id, SUM(count) AS total FROM rate_limit_counters
         WHERE window_minute = $1 GROUP BY actor_id",
    )
    .bind(window_minute)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<String, _>("actor_id"), r.get::<i64, _>("total")))
        .collect())
}

/// Drop rows for windows older than `keep_minute` (called every sync tick to bound table growth —
/// a few minutes of history is more than enough since only the current window is ever read).
pub async fn prune_before(pool: &AnyPool, keep_minute: i64) -> Result<u64> {
    let res = sqlx::query("DELETE FROM rate_limit_counters WHERE window_minute < $1")
        .bind(keep_minute)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    async fn pool() -> AnyPool {
        let db = format!(
            "{}/atlas-rate-limit-{}-{}.db",
            std::env::temp_dir().display(),
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst),
        );
        let _ = std::fs::remove_file(&db);
        let url = format!("sqlite://{db}?mode=rwc");
        let p = crate::connect(&url).await.unwrap();
        crate::migrate(&p, &url).await.unwrap();
        p
    }

    #[tokio::test]
    async fn cluster_totals_sum_across_replicas_for_the_same_window() {
        let p = pool().await;
        upsert_replica_count(&p, "alice", 100, "replica-a", 40)
            .await
            .unwrap();
        upsert_replica_count(&p, "alice", 100, "replica-b", 35)
            .await
            .unwrap();
        upsert_replica_count(&p, "bob", 100, "replica-a", 5)
            .await
            .unwrap();
        // A different window must not be summed in.
        upsert_replica_count(&p, "alice", 99, "replica-a", 999)
            .await
            .unwrap();

        let totals = cluster_totals(&p, 100).await.unwrap();
        let alice = totals.iter().find(|(a, _)| a == "alice").unwrap().1;
        let bob = totals.iter().find(|(a, _)| a == "bob").unwrap().1;
        assert_eq!(alice, 75, "40 (replica-a) + 35 (replica-b)");
        assert_eq!(bob, 5);
    }

    #[tokio::test]
    async fn upsert_overwrites_not_adds() {
        let p = pool().await;
        upsert_replica_count(&p, "alice", 100, "replica-a", 10)
            .await
            .unwrap();
        upsert_replica_count(&p, "alice", 100, "replica-a", 12)
            .await
            .unwrap();
        let totals = cluster_totals(&p, 100).await.unwrap();
        assert_eq!(
            totals[0].1, 12,
            "second sync overwrites, doesn't add to, the first"
        );
    }

    #[tokio::test]
    async fn prune_drops_only_old_windows() {
        let p = pool().await;
        upsert_replica_count(&p, "alice", 90, "replica-a", 1)
            .await
            .unwrap();
        upsert_replica_count(&p, "alice", 100, "replica-a", 1)
            .await
            .unwrap();
        let pruned = prune_before(&p, 100).await.unwrap();
        assert_eq!(pruned, 1);
        assert!(cluster_totals(&p, 90).await.unwrap().is_empty());
        assert_eq!(cluster_totals(&p, 100).await.unwrap().len(), 1);
    }
}
