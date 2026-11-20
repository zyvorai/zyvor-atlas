// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Cutover records (freeze source, drain CDC lag, switch endpoint, rollback window).

use anyhow::Result;
use atlas_api_types::Cutover;
use sqlx::{AnyPool, Row};

#[allow(clippy::too_many_arguments)]
pub async fn insert_cutover(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    plan_id: &str,
    from_endpoint: Option<&str>,
    to_endpoint: Option<&str>,
    drain_deadline: Option<&str>,
    rollback_deadline: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO cutovers
         (id, tenant_id, plan_id, from_endpoint, to_endpoint, drain_deadline, rollback_deadline, state)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'freezing')",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(plan_id)
    .bind(from_endpoint)
    .bind(to_endpoint)
    .bind(drain_deadline)
    .bind(rollback_deadline)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_state(pool: &AnyPool, id: &str, state: &str) -> Result<()> {
    sqlx::query("UPDATE cutovers SET state=$1 WHERE id=$2")
        .bind(state)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_complete(pool: &AnyPool, id: &str, state: &str) -> Result<()> {
    sqlx::query("UPDATE cutovers SET state=$1, completed_at=$2 WHERE id=$3")
        .bind(state)
        .bind(crate::now_rfc3339(chrono::Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_cutover(pool: &AnyPool, id: &str) -> Result<Option<Cutover>> {
    let row = sqlx::query(select("WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_cutover))
}

pub async fn list_cutovers(pool: &AnyPool) -> Result<Vec<Cutover>> {
    let rows = sqlx::query(select("ORDER BY created_at DESC"))
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row_to_cutover).collect())
}

pub async fn list_by_state(pool: &AnyPool, state: &str) -> Result<Vec<Cutover>> {
    let rows = sqlx::query(select("WHERE state = $1 ORDER BY created_at"))
        .bind(state)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row_to_cutover).collect())
}

pub async fn latest_for_plan(pool: &AnyPool, plan_id: &str) -> Result<Option<Cutover>> {
    let row = sqlx::query(select(
        "WHERE plan_id = $1 ORDER BY created_at DESC LIMIT 1",
    ))
    .bind(plan_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_cutover))
}

// `tail` is only ever a string literal from this module's own call sites (never user input — user
// data always goes through `$N` bind parameters), so asserting the assembled SQL safe is sound.
fn select(tail: &str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT id, tenant_id, plan_id, state, from_endpoint, to_endpoint, drain_deadline,
                rollback_deadline, created_at, completed_at
         FROM cutovers {tail}"
    ))
}

fn row_to_cutover(r: sqlx::any::AnyRow) -> Cutover {
    Cutover {
        id: r.get("id"),
        tenant_id: r.get("tenant_id"),
        plan_id: r.get("plan_id"),
        state: r.get("state"),
        from_endpoint: r.get("from_endpoint"),
        to_endpoint: r.get("to_endpoint"),
        drain_deadline: r.get("drain_deadline"),
        rollback_deadline: r.get("rollback_deadline"),
        created_at: r.get("created_at"),
        completed_at: r.get("completed_at"),
    }
}
