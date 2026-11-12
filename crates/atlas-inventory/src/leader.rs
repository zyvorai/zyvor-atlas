// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! DB-backed leader election (day-2 HA). A single `leader_lease` row per lease name is held by one
//! instance at a time; the holder renews it before it expires. On single-replica SQLite this is a
//! no-op (this instance always wins), but on a shared DB it ensures only the leader runs the
//! periodic workers so scheduled jobs aren't double-fired.

use anyhow::Result;
use sqlx::AnyPool;

use crate::now_rfc3339;

/// Try to acquire or renew the `name` lease for `holder`, valid for `ttl_secs`. Returns whether this
/// holder now owns it. The upsert only takes the lease when it is expired or already ours, so a live
/// lease held by another instance is left alone.
pub async fn try_acquire(pool: &AnyPool, name: &str, holder: &str, ttl_secs: i64) -> Result<bool> {
    let now = chrono::Utc::now();
    let expires_at = now_rfc3339(now + chrono::Duration::seconds(ttl_secs.max(1)));
    sqlx::query(
        "INSERT INTO leader_lease (name, holder, expires_at)
         VALUES ($1, $2, $3)
         ON CONFLICT(name) DO UPDATE SET holder=excluded.holder, expires_at=excluded.expires_at
         WHERE leader_lease.expires_at < $4
            OR leader_lease.holder = excluded.holder",
    )
    .bind(name)
    .bind(holder)
    .bind(expires_at)
    .bind(now_rfc3339(now))
    .execute(pool)
    .await?;
    let current: Option<String> =
        sqlx::query_scalar("SELECT holder FROM leader_lease WHERE name=$1")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    Ok(current.as_deref() == Some(holder))
}

/// The current holder of a lease (for observability), if any.
pub async fn holder(pool: &AnyPool, name: &str) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT holder FROM leader_lease WHERE name=$1")
            .bind(name)
            .fetch_optional(pool)
            .await?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    async fn pool() -> AnyPool {
        let db = format!(
            "{}/atlas-leader-{}-{}.db",
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
    async fn lease_acquire_renew_and_takeover() {
        let p = pool().await;
        assert!(
            try_acquire(&p, "workers", "A", 30).await.unwrap(),
            "A acquires a free lease"
        );
        assert!(
            try_acquire(&p, "workers", "A", 30).await.unwrap(),
            "A renews its own lease"
        );
        assert!(
            !try_acquire(&p, "workers", "B", 30).await.unwrap(),
            "B blocked while A's lease is valid"
        );

        // A renews with a 1s TTL, then lets it lapse; B takes over the expired lease.
        assert!(try_acquire(&p, "workers", "A", 1).await.unwrap());
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert!(
            try_acquire(&p, "workers", "B", 30).await.unwrap(),
            "B takes over an expired lease"
        );
        assert_eq!(holder(&p, "workers").await.unwrap().as_deref(), Some("B"));
    }
}
