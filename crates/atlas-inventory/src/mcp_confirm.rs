// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Durable single-use record of MCP write confirmation tokens (`migrations/0039_mcp_confirm_used.sql`).
//! The gateway redeems a token by inserting its `jti`; the primary key makes the first insert win on every
//! replica, so "already used" survives a restart and holds across replicas that share the database.

use anyhow::Result;
use sqlx::AnyPool;

/// Record `jti` as used. `Ok(true)` if this call was the first to use it, `Ok(false)` if it was already
/// recorded (a unique violation). Any other database error is returned so the caller can fail closed.
pub async fn mark_used(pool: &AnyPool, jti: &str, exp: i64) -> Result<bool> {
    match sqlx::query("INSERT INTO mcp_confirm_used (jti, exp) VALUES ($1, $2)")
        .bind(jti)
        .bind(exp)
        .execute(pool)
        .await
    {
        Ok(_) => Ok(true),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Drop rows whose token expired before `before` (unix seconds). Returns the number removed.
pub async fn prune_expired(pool: &AnyPool, before: i64) -> Result<u64> {
    let res = sqlx::query("DELETE FROM mcp_confirm_used WHERE exp < $1")
        .bind(before)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_use_wins_and_expired_rows_are_pruned() {
        let db = format!(
            "{}/atlas-mcp-confirm-{}.db",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = std::fs::remove_file(&db);
        let url = format!("sqlite://{db}?mode=rwc");
        let pool = crate::connect(&url).await.unwrap();
        crate::migrate(&pool, &url).await.unwrap();

        assert!(mark_used(&pool, "jti-a", 1_000).await.unwrap());
        assert!(
            !mark_used(&pool, "jti-a", 1_000).await.unwrap(),
            "second use is a unique violation"
        );
        assert!(mark_used(&pool, "jti-b", 5_000).await.unwrap());
        assert_eq!(prune_expired(&pool, 2_000).await.unwrap(), 1);
        // The pruned id can be recorded again; the live one cannot.
        assert!(mark_used(&pool, "jti-a", 1_000).await.unwrap());
        assert!(!mark_used(&pool, "jti-b", 5_000).await.unwrap());
        let _ = std::fs::remove_file(&db);
    }
}
