// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! CDC stream records (Debezium source->edge replication + live lag).

use anyhow::Result;
use atlas_api_types::CdcStream;
use sqlx::{AnyPool, Row};

#[allow(clippy::too_many_arguments)]
pub async fn insert_stream(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    plan_id: &str,
    engine: &str,
    connect_name: &str,
    connector_name: &str,
    topic_prefix: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO cdc_streams
         (id, tenant_id, plan_id, engine, connect_name, connector_name, topic_prefix, state)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'starting')",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(plan_id)
    .bind(engine)
    .bind(connect_name)
    .bind(connector_name)
    .bind(topic_prefix)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_state(pool: &AnyPool, id: &str, state: &str) -> Result<()> {
    sqlx::query("UPDATE cdc_streams SET state=$1 WHERE id=$2")
        .bind(state)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Increment a stream's restart counter (day-2 self-heal) and return the new count.
pub async fn bump_restart(pool: &AnyPool, id: &str) -> Result<i64> {
    sqlx::query("UPDATE cdc_streams SET restart_count = restart_count + 1 WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await?;
    let n: i64 = sqlx::query_scalar("SELECT restart_count FROM cdc_streams WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(n)
}

/// Update the live replication-lag fields (called by the reconciler each tick).
#[allow(clippy::too_many_arguments)]
pub async fn update_lag(
    pool: &AnyPool,
    id: &str,
    lag_bytes: i64,
    lag_seconds: i64,
    last_source_lsn: Option<&str>,
    last_applied_lsn: Option<&str>,
    events_total: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE cdc_streams SET lag_bytes=$1, lag_seconds=$2, last_source_lsn=$3, last_applied_lsn=$4,
         events_total=$5, lag_updated_at=$6 WHERE id=$7",
    )
    .bind(lag_bytes)
    .bind(lag_seconds)
    .bind(last_source_lsn)
    .bind(last_applied_lsn)
    .bind(events_total)
    .bind(crate::now_rfc3339(chrono::Utc::now()))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_stream(pool: &AnyPool, id: &str) -> Result<Option<CdcStream>> {
    let row = sqlx::query(select("WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_stream))
}

pub async fn list_streams(pool: &AnyPool) -> Result<Vec<CdcStream>> {
    let rows = sqlx::query(select("ORDER BY created_at DESC"))
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row_to_stream).collect())
}

pub async fn list_by_state(pool: &AnyPool, state: &str) -> Result<Vec<CdcStream>> {
    let rows = sqlx::query(select("WHERE state = $1 ORDER BY created_at DESC"))
        .bind(state)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row_to_stream).collect())
}

// `tail` is only ever a string literal from this module's own call sites (never user input — user
// data always goes through `$N` bind parameters), so asserting the assembled SQL safe is sound.
fn select(tail: &str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT id, tenant_id, plan_id, engine, connect_name, connector_name, topic_prefix, state,
                lag_bytes, lag_seconds, last_source_lsn, last_applied_lsn, events_total,
                lag_updated_at, restart_count, created_at
         FROM cdc_streams {tail}"
    ))
}

fn row_to_stream(r: sqlx::any::AnyRow) -> CdcStream {
    CdcStream {
        id: r.get("id"),
        tenant_id: r.get("tenant_id"),
        plan_id: r.get("plan_id"),
        engine: r.get("engine"),
        connect_name: r.get("connect_name"),
        connector_name: r.get("connector_name"),
        topic_prefix: r.get("topic_prefix"),
        state: r.get("state"),
        lag_bytes: r.get("lag_bytes"),
        lag_seconds: r.get("lag_seconds"),
        last_source_lsn: r.get("last_source_lsn"),
        last_applied_lsn: r.get("last_applied_lsn"),
        events_total: r.get("events_total"),
        lag_updated_at: r.get("lag_updated_at"),
        restart_count: r.get("restart_count"),
        created_at: r.get("created_at"),
    }
}
