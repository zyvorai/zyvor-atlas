// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Token revocation deny-list (day-2 governance). A revoked JWT id (`jti`) is rejected by the auth
//! middleware, so a leaked or rotated service-account credential can be killed before its TTL.

use anyhow::Result;
use sqlx::{AnyPool, Row};

use crate::now_rfc3339;

/// Revoke a token by its `jti`. Idempotent. Self-prunes revocations older than 30 days (token TTLs
/// are capped well under that, so any such token has certainly expired and no longer needs listing).
pub async fn revoke(pool: &AnyPool, jti: &str, by: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO revoked_tokens (jti, revoked_by) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(jti)
    .bind(by)
    .execute(pool)
    .await?;
    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
    sqlx::query("DELETE FROM revoked_tokens WHERE revoked_at < $1")
        .bind(now_rfc3339(cutoff))
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether a token id has been revoked. Empty `jti` (legacy tokens) is never revoked.
pub async fn is_revoked(pool: &AnyPool, jti: &str) -> Result<bool> {
    if jti.is_empty() {
        return Ok(false);
    }
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM revoked_tokens WHERE jti = $1")
        .bind(jti)
        .fetch_one(pool)
        .await?;
    Ok(n > 0)
}

/// List the current revocations (newest first).
pub async fn list(pool: &AnyPool) -> Result<Vec<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT jti, revoked_by, revoked_at FROM revoked_tokens ORDER BY revoked_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "jti": r.get::<String, _>("jti"),
                "revoked_by": r.get::<Option<String>, _>("revoked_by"),
                "revoked_at": r.get::<String, _>("revoked_at"),
            })
        })
        .collect())
}
