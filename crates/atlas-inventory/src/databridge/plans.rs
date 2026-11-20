// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Migration plan records (source -> edge pipeline state).

use anyhow::Result;
use atlas_api_types::MigrationPlan;
use sqlx::{AnyPool, Row};

pub async fn insert_plan(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    name: &str,
    source_id: &str,
    rollback_window_secs: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO migration_plans (id, tenant_id, name, source_id, rollback_window_secs, state)
         VALUES ($1, $2, $3, $4, $5, 'draft')",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(name)
    .bind(source_id)
    .bind(rollback_window_secs)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_state(pool: &AnyPool, id: &str, state: &str) -> Result<()> {
    sqlx::query("UPDATE migration_plans SET state=$1 WHERE id=$2")
        .bind(state)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Atomically move a plan from `from_state` to `to_state`. Returns whether the transition took
/// effect. Guards stage-entry points (cutover, rollback) that are re-checked at request time but
/// then run as an async job: two racing requests (double-click, client retry) can both pass the
/// route-level state check before either job runs, so the job itself re-validates the precondition
/// with this atomic `UPDATE ... WHERE state=...` — only the first to land wins, the second sees
/// `false` and bails instead of duplicating a real cutover/rollback against the source database.
pub async fn try_transition(
    pool: &AnyPool,
    id: &str,
    from_state: &str,
    to_state: &str,
) -> Result<bool> {
    let res = sqlx::query("UPDATE migration_plans SET state=$1 WHERE id=$2 AND state=$3")
        .bind(to_state)
        .bind(id)
        .bind(from_state)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn set_assessment(
    pool: &AnyPool,
    id: &str,
    readiness_score: i64,
    assessment: &serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "UPDATE migration_plans SET readiness_score=$1, assessment=$2, state='assessed' WHERE id=$3",
    )
    .bind(readiness_score)
    .bind(assessment.to_string())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_edge_cluster(pool: &AnyPool, id: &str, edge_cluster_id: &str) -> Result<()> {
    sqlx::query("UPDATE migration_plans SET edge_cluster_id=$1 WHERE id=$2")
        .bind(edge_cluster_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_cdc_stream(pool: &AnyPool, id: &str, cdc_stream_id: &str) -> Result<()> {
    sqlx::query("UPDATE migration_plans SET cdc_stream_id=$1 WHERE id=$2")
        .bind(cdc_stream_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_cutover_at(pool: &AnyPool, id: &str, at: &str) -> Result<()> {
    sqlx::query("UPDATE migration_plans SET cutover_at=$1 WHERE id=$2")
        .bind(at)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_plan_row(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM migration_plans WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_plan(pool: &AnyPool, id: &str) -> Result<Option<MigrationPlan>> {
    let row = sqlx::query(select("WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_plan))
}

pub async fn list_plans(pool: &AnyPool) -> Result<Vec<MigrationPlan>> {
    let rows = sqlx::query(select("ORDER BY created_at DESC"))
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row_to_plan).collect())
}

/// Plans in a given pipeline state — used by the reconciler to advance in-flight work.
pub async fn list_by_state(pool: &AnyPool, state: &str) -> Result<Vec<MigrationPlan>> {
    let rows = sqlx::query(select("WHERE state = $1 ORDER BY created_at DESC"))
        .bind(state)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row_to_plan).collect())
}

/// Plans referencing a source — used to block deleting a source out from under a live migration
/// (the FK is `ON DELETE CASCADE`, so an unguarded delete silently destroys the plan's history).
pub async fn list_for_source(pool: &AnyPool, source_id: &str) -> Result<Vec<MigrationPlan>> {
    let rows = sqlx::query(select("WHERE source_id = $1 ORDER BY created_at DESC"))
        .bind(source_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row_to_plan).collect())
}

// `tail` is only ever a string literal from this module's own call sites (never user input — user
// data always goes through `$N` bind parameters), so asserting the assembled SQL safe is sound.
fn select(tail: &str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT id, tenant_id, name, source_id, edge_cluster_id, cdc_stream_id, readiness_score,
                assessment, rollback_window_secs, cutover_at, state, created_at
         FROM migration_plans {tail}"
    ))
}

fn row_to_plan(r: sqlx::any::AnyRow) -> MigrationPlan {
    let assessment: String = r.get("assessment");
    MigrationPlan {
        id: r.get("id"),
        tenant_id: r.get("tenant_id"),
        name: r.get("name"),
        source_id: r.get("source_id"),
        edge_cluster_id: r.get("edge_cluster_id"),
        cdc_stream_id: r.get("cdc_stream_id"),
        readiness_score: r.get("readiness_score"),
        assessment: serde_json::from_str(&assessment).unwrap_or(serde_json::Value::Null),
        rollback_window_secs: r.get("rollback_window_secs"),
        cutover_at: r.get("cutover_at"),
        state: r.get("state"),
        created_at: r.get("created_at"),
    }
}
