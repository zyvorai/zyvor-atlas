// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Per-sink alert delivery tracking (PagerDuty/Opsgenie/Slack — `crates/atlas-monitor/src/notify/`).
//! Separate from `alerts::list_unnotified_open`/`mark_notified`, which remain the pre-existing
//! generic webhook's own single-sink tracking (`storage_alerts.notified_at`), untouched here —
//! see `migrations/0030_alert_notifications.sql` for why a several-sinks-per-alert table replaces
//! a fixed set of extra columns.

use anyhow::Result;
use atlas_api_types::AlertRecord;
use sqlx::AnyPool;

use crate::alerts::row_to_alert;
use crate::now_rfc3339;

const SELECT_COLS: &str = "id, severity, source, resource_type, resource_id, title, description, \
    evidence, state, created_at, resolved_at, acknowledged_at, acknowledged_by, silenced_until";

/// Open, not-silenced alerts not yet trigger-notified for `sink`.
pub async fn pending_triggers(pool: &AnyPool, sink: &str) -> Result<Vec<AlertRecord>> {
    // Only the `SELECT_COLS` const is interpolated (never user input); values are `$N` binds.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_COLS} FROM storage_alerts
         WHERE state='open'
           AND (silenced_until IS NULL OR silenced_until < $1)
           AND id NOT IN (SELECT alert_id FROM alert_notifications WHERE sink=$2 AND event='trigger')
         ORDER BY created_at ASC"
    )))
    .bind(now_rfc3339(chrono::Utc::now()))
    .bind(sink)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_alert).collect())
}

/// Resolved alerts that were trigger-notified for `sink` but not yet resolve-notified — lets
/// PagerDuty/Opsgenie auto-close the incident instead of an operator resolving it by hand there
/// too. Only fires for alerts the sink actually knew about (skips ones that resolved before the
/// sink was ever enabled).
pub async fn pending_resolves(pool: &AnyPool, sink: &str) -> Result<Vec<AlertRecord>> {
    // Only the `SELECT_COLS` const is interpolated (never user input); values are `$N` binds.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_COLS} FROM storage_alerts
         WHERE state='resolved'
           AND id IN (SELECT alert_id FROM alert_notifications WHERE sink=$1 AND event='trigger')
           AND id NOT IN (SELECT alert_id FROM alert_notifications WHERE sink=$2 AND event='resolve')
         ORDER BY resolved_at ASC"
    )))
    .bind(sink)
    .bind(sink)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_alert).collect())
}

/// Record that `event` ("trigger" or "resolve") was successfully delivered to `sink` for `alert_id`.
pub async fn mark_sent(pool: &AnyPool, alert_id: &str, sink: &str, event: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO alert_notifications (alert_id, sink, event) VALUES ($1, $2, $3)
         ON CONFLICT(alert_id, sink, event) DO NOTHING",
    )
    .bind(alert_id)
    .bind(sink)
    .bind(event)
    .execute(pool)
    .await?;
    Ok(())
}
