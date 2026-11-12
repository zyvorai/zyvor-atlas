// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Alert records (PDF §15.2). Alerts are keyed by a deterministic id (rule + resource) so the
//! monitor upserts rather than duplicating on each tick.

use anyhow::Result;
use atlas_api_types::AlertRecord;
use sqlx::{AnyPool, Row};

use crate::now_rfc3339;

/// Insert or re-open an alert (deterministic id per rule+resource).
#[allow(clippy::too_many_arguments)]
pub async fn upsert_open(
    pool: &AnyPool,
    id: &str,
    severity: &str,
    source: &str,
    resource_type: &str,
    resource_id: &str,
    title: &str,
    description: &str,
    evidence: &serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO storage_alerts (id, severity, source, resource_type, resource_id, title, description, evidence, state)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'open')
         ON CONFLICT(id) DO UPDATE SET
            severity=excluded.severity, title=excluded.title, description=excluded.description,
            evidence=excluded.evidence, state='open', resolved_at=NULL,
            -- re-arm the webhook only when a *resolved* alert re-opens; leave it set while still open.
            notified_at=CASE WHEN storage_alerts.state='resolved' THEN NULL ELSE storage_alerts.notified_at END",
    )
    .bind(id)
    .bind(severity)
    .bind(source)
    .bind(resource_type)
    .bind(resource_id)
    .bind(title)
    .bind(description)
    .bind(evidence.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// Resolve an alert if it exists and is currently open.
pub async fn resolve(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE storage_alerts SET state='resolved', resolved_at=$1
         WHERE id=$2 AND state='open'",
    )
    .bind(now_rfc3339(chrono::Utc::now()))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list(pool: &AnyPool, state: Option<&str>) -> Result<Vec<AlertRecord>> {
    let rows = match state {
        Some(st) => {
            sqlx::query(select("WHERE state = $1 ORDER BY created_at DESC"))
                .bind(st)
                .fetch_all(pool)
                .await?
        }
        None => {
            sqlx::query(select("ORDER BY created_at DESC"))
                .fetch_all(pool)
                .await?
        }
    };
    Ok(rows.into_iter().map(row_to_alert).collect())
}

/// Open alerts that have not yet been pushed to the webhook (drives the notifier). Silenced alerts
/// (silence window still in the future) are skipped so an operator can mute known-noisy conditions.
pub async fn list_unnotified_open(pool: &AnyPool) -> Result<Vec<AlertRecord>> {
    let rows = sqlx::query(select(
        "WHERE state='open' AND notified_at IS NULL \
         AND (silenced_until IS NULL OR silenced_until < $1) \
         ORDER BY created_at ASC",
    ))
    .bind(now_rfc3339(chrono::Utc::now()))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_alert).collect())
}

/// Acknowledge an alert (records who saw it; does not resolve it).
pub async fn acknowledge(pool: &AnyPool, id: &str, by: &str) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE storage_alerts SET acknowledged_at=$1,
         acknowledged_by=$2 WHERE id=$3",
    )
    .bind(now_rfc3339(chrono::Utc::now()))
    .bind(by)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Silence an alert's webhook notification for `secs` seconds from now. The condition keeps being
/// tracked; only paging is suppressed.
pub async fn silence(pool: &AnyPool, id: &str, secs: i64) -> Result<bool> {
    let until = now_rfc3339(chrono::Utc::now() + chrono::Duration::seconds(secs));
    let res = sqlx::query("UPDATE storage_alerts SET silenced_until=$1 WHERE id=$2")
        .bind(until)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Manually resolve an alert regardless of state (operator override). Returns whether a row changed.
pub async fn resolve_manual(pool: &AnyPool, id: &str) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE storage_alerts SET state='resolved', resolved_at=$1
         WHERE id=$2 AND state='open'",
    )
    .bind(now_rfc3339(chrono::Utc::now()))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Stamp an alert as notified so it isn't pushed again while it stays open.
pub async fn mark_notified(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query("UPDATE storage_alerts SET notified_at=$1 WHERE id=$2")
        .bind(now_rfc3339(chrono::Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn exists(pool: &AnyPool, id: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage_alerts WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(n > 0)
}

pub async fn count_open(pool: &AnyPool) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM storage_alerts WHERE state='open'")
            .fetch_one(pool)
            .await?,
    )
}

// `tail` is only ever a string literal from this module's own call sites (never user input — user
// data always goes through `$N` bind parameters), so asserting the assembled SQL safe is sound.
fn select(tail: &str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT id, severity, source, resource_type, resource_id, title, description, evidence, state, \
                created_at, resolved_at, acknowledged_at, acknowledged_by, silenced_until
         FROM storage_alerts {tail}"
    ))
}

pub(crate) fn row_to_alert(r: sqlx::any::AnyRow) -> AlertRecord {
    let evidence: serde_json::Value = serde_json::from_str(r.get::<String, _>("evidence").as_str())
        .unwrap_or(serde_json::Value::Null);
    AlertRecord {
        id: r.get("id"),
        severity: r.get("severity"),
        source: r.get("source"),
        resource_type: r.get("resource_type"),
        resource_id: r.get("resource_id"),
        title: r.get("title"),
        description: r.get("description"),
        evidence,
        state: r.get("state"),
        created_at: r.get("created_at"),
        resolved_at: r.get("resolved_at"),
        acknowledged_at: r.get("acknowledged_at"),
        acknowledged_by: r.get("acknowledged_by"),
        silenced_until: r.get("silenced_until"),
    }
}
