// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Protection schedules: periodic snapshot policy per volume (PDF §12.3). The scheduler worker
//! (in `atlas-jobs`) reads due schedules, enqueues snapshot jobs, and advances `next_run_at`.

use anyhow::Result;
use atlas_api_types::SnapshotSchedule;
use sqlx::{AnyPool, Row};

use crate::now_rfc3339;

// `tail` is only ever a string literal from this module's own call sites (never user input — user
// data always goes through `$N` bind parameters), so asserting the assembled SQL safe is sound.
fn select(tail: &str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT id, tenant_id, volume_id, kind, bucket_id, mode, interval_secs, keep, enabled,
                last_run_at, next_run_at, created_at
         FROM snapshot_schedules {tail}"
    ))
}

fn row_to_schedule(r: sqlx::any::AnyRow) -> SnapshotSchedule {
    SnapshotSchedule {
        id: r.get("id"),
        tenant_id: r.get("tenant_id"),
        volume_id: r.get("volume_id"),
        kind: r.get("kind"),
        bucket_id: r.get("bucket_id"),
        mode: r.get("mode"),
        interval_secs: r.get("interval_secs"),
        keep: r.get("keep"),
        enabled: r.get::<i64, _>("enabled") != 0,
        last_run_at: r.get("last_run_at"),
        next_run_at: r.get("next_run_at"),
        created_at: r.get("created_at"),
    }
}

/// Create a schedule. `next_run_at` starts one interval from now so the first run is not immediate.
/// `kind` is "snapshot" or "backup"; backups also carry a `bucket_id` + `mode`. Returns the row.
#[allow(clippy::too_many_arguments)]
pub async fn insert(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    volume_id: &str,
    kind: &str,
    bucket_id: Option<&str>,
    mode: &str,
    interval_secs: i64,
    keep: i64,
) -> Result<SnapshotSchedule> {
    let next = now_rfc3339(chrono::Utc::now() + chrono::Duration::seconds(interval_secs));
    sqlx::query(
        "INSERT INTO snapshot_schedules
            (id, tenant_id, volume_id, kind, bucket_id, mode, interval_secs, keep, next_run_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(volume_id)
    .bind(kind)
    .bind(bucket_id)
    .bind(mode)
    .bind(interval_secs)
    .bind(keep)
    .bind(&next)
    .execute(pool)
    .await?;
    Ok(get(pool, id).await?.expect("schedule just inserted"))
}

pub async fn get(pool: &AnyPool, id: &str) -> Result<Option<SnapshotSchedule>> {
    let row = sqlx::query(select("WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_schedule))
}

pub async fn list(pool: &AnyPool, volume_id: Option<&str>) -> Result<Vec<SnapshotSchedule>> {
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
    Ok(rows.into_iter().map(row_to_schedule).collect())
}

pub async fn delete(pool: &AnyPool, id: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM snapshot_schedules WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Enabled schedules whose `next_run_at` is in the past (i.e. due to run now).
pub async fn due(pool: &AnyPool) -> Result<Vec<SnapshotSchedule>> {
    let rows = sqlx::query(select(
        "WHERE enabled = 1 AND next_run_at <= $1 ORDER BY next_run_at ASC",
    ))
    .bind(now_rfc3339(chrono::Utc::now()))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_schedule).collect())
}

/// Record that a schedule just ran: set `last_run_at = now`, advance `next_run_at` by its
/// interval. `interval_secs` is read back into Rust first (rather than computed server-side via
/// `strftime`/interval arithmetic on the stored column) so the "now + offset" math stays in one
/// place (`now_rfc3339`) instead of forking per backend for this one case.
pub async fn mark_ran(pool: &AnyPool, id: &str) -> Result<()> {
    let interval_secs: Option<i64> =
        sqlx::query_scalar("SELECT interval_secs FROM snapshot_schedules WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let Some(interval_secs) = interval_secs else {
        return Ok(());
    };
    let now = chrono::Utc::now();
    let next = now_rfc3339(now + chrono::Duration::seconds(interval_secs));
    sqlx::query("UPDATE snapshot_schedules SET last_run_at = $1, next_run_at = $2 WHERE id = $3")
        .bind(now_rfc3339(now))
        .bind(next)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
