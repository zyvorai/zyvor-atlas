// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Agent workspaces: the catalog side of disposable clones (migration 0037). The gateway does the
//! snapshot/clone/delete against the backend; this records which clone belongs to which run and when
//! it expires. State changes are compare-and-set so two gateway replicas (Postgres) never both act
//! on the same workspace.

use anyhow::Result;
use serde::Serialize;
use sqlx::{any::AnyRow, AnyPool, Row};

pub const ACTIVE: &str = "active";
pub const RESETTING: &str = "resetting";
/// A reset that failed after the old clone was deleted: no volume, base snapshot still held.
pub const RESET_FAILED: &str = "reset_failed";
/// Being torn down (or a teardown that failed part-way and is retried by the GC).
pub const DISCARDING: &str = "discarding";

#[derive(Debug, Clone, Serialize)]
pub struct Workspace {
    pub id: String,
    pub tenant_id: String,
    pub owner: String,
    pub name: String,
    pub source_volume_id: String,
    pub base_snapshot_id: String,
    pub owns_base_snapshot: bool,
    pub volume_id: String,
    pub max_bytes: i64,
    pub state: String,
    pub reset_count: i64,
    pub last_error: Option<String>,
    pub created_at_unix: i64,
    pub expires_at_unix: i64,
}

const COLS: &str = "id, tenant_id, owner, name, source_volume_id, base_snapshot_id, \
                    owns_base_snapshot, volume_id, max_bytes, state, reset_count, last_error, \
                    created_at_unix, expires_at_unix";

fn row(r: AnyRow) -> Workspace {
    Workspace {
        id: r.get("id"),
        tenant_id: r.get("tenant_id"),
        owner: r.get("owner"),
        name: r.get("name"),
        source_volume_id: r.get("source_volume_id"),
        base_snapshot_id: r.get("base_snapshot_id"),
        owns_base_snapshot: r.get::<i64, _>("owns_base_snapshot") != 0,
        volume_id: r.get("volume_id"),
        max_bytes: r.get("max_bytes"),
        state: r.get("state"),
        reset_count: r.get("reset_count"),
        last_error: r.get("last_error"),
        created_at_unix: r.get("created_at_unix"),
        expires_at_unix: r.get("expires_at_unix"),
    }
}

pub async fn insert(pool: &AnyPool, w: &Workspace) -> Result<()> {
    sqlx::query(
        "INSERT INTO agent_workspaces (id, tenant_id, owner, name, source_volume_id, base_snapshot_id,
             owns_base_snapshot, volume_id, max_bytes, state, reset_count, created_at_unix, expires_at_unix)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 0, $11, $12)",
    )
    .bind(&w.id)
    .bind(&w.tenant_id)
    .bind(&w.owner)
    .bind(&w.name)
    .bind(&w.source_volume_id)
    .bind(&w.base_snapshot_id)
    .bind(w.owns_base_snapshot as i64)
    .bind(&w.volume_id)
    .bind(w.max_bytes)
    .bind(&w.state)
    .bind(w.created_at_unix)
    .bind(w.expires_at_unix)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get(pool: &AnyPool, id: &str) -> Result<Option<Workspace>> {
    // `COLS` is a const column list; the id goes through a bind.
    let q = sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM agent_workspaces WHERE id = $1"));
    Ok(sqlx::query(q).bind(id).fetch_optional(pool).await?.map(row))
}

/// Workspaces newest first; `tenant` limits to one tenant (`None` = every tenant).
pub async fn list(pool: &AnyPool, tenant: Option<&str>) -> Result<Vec<Workspace>> {
    let rows = match tenant {
        Some(t) => {
            // Const column list; tenant is a bind.
            let q = sqlx::AssertSqlSafe(format!(
                "SELECT {COLS} FROM agent_workspaces WHERE tenant_id = $1 ORDER BY created_at_unix DESC, id"
            ));
            sqlx::query(q).bind(t).fetch_all(pool).await?
        }
        None => {
            // Const column list, no user input.
            let q = sqlx::AssertSqlSafe(format!(
                "SELECT {COLS} FROM agent_workspaces ORDER BY created_at_unix DESC, id"
            ));
            sqlx::query(q).fetch_all(pool).await?
        }
    };
    Ok(rows.into_iter().map(row).collect())
}

/// Workspaces the GC should act on at `now`: active ones past their expiry, plus any teardown that
/// was left half-done.
pub async fn due_for_gc(pool: &AnyPool, now_unix: i64) -> Result<Vec<Workspace>> {
    // Const column list; the state names and the time are binds.
    let q = sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM agent_workspaces
         WHERE state = $1 OR (state IN ($2, $3, $4) AND expires_at_unix <= $5)
         ORDER BY expires_at_unix, id"
    ));
    let rows = sqlx::query(q)
        .bind(DISCARDING)
        .bind(ACTIVE)
        .bind(RESET_FAILED)
        .bind(RESETTING)
        .bind(now_unix)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row).collect())
}

/// Workspaces of a tenant that still hold resources (every row; a finished teardown deletes its row).
pub async fn count_for_tenant(pool: &AnyPool, tenant_id: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_workspaces WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(pool)
            .await?,
    )
}

/// Sum of `max_bytes` over every workspace an owner still holds in a tenant (a finished teardown
/// deletes its row, so every row is live): the quantity a per-agent byte budget is checked against.
/// A workspace created without a cap counts 0 here, which is why a budget makes the cap mandatory.
pub async fn live_max_bytes_for_owner(pool: &AnyPool, tenant_id: &str, owner: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(max_bytes), 0) AS BIGINT) FROM agent_workspaces
         WHERE tenant_id = $1 AND owner = $2",
    )
    .bind(tenant_id)
    .bind(owner)
    .fetch_one(pool)
    .await?)
}

/// Move `id` from one of `from` to `to`; true when this caller made the change (so exactly one of
/// several racing callers proceeds).
pub async fn transition(pool: &AnyPool, id: &str, from: &[&str], to: &str) -> Result<bool> {
    for f in from {
        let n = sqlx::query("UPDATE agent_workspaces SET state = $1 WHERE id = $2 AND state = $3")
            .bind(to)
            .bind(id)
            .bind(*f)
            .execute(pool)
            .await?
            .rows_affected();
        if n == 1 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// After a successful reset: the new clone, back to `active`. Only applies while the workspace is
/// still `resetting` (the GC may have claimed it for teardown in the meantime); false when it did not.
pub async fn set_reset_done(pool: &AnyPool, id: &str, volume_id: &str) -> Result<bool> {
    let n = sqlx::query(
        "UPDATE agent_workspaces SET volume_id = $1, state = $2, reset_count = reset_count + 1,
             last_error = NULL WHERE id = $3 AND state = $4",
    )
    .bind(volume_id)
    .bind(ACTIVE)
    .bind(id)
    .bind(RESETTING)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// A reset whose re-clone failed: no volume until a later reset succeeds (same `resetting` guard).
pub async fn set_reset_failed(pool: &AnyPool, id: &str, error: &str) -> Result<()> {
    sqlx::query(
        "UPDATE agent_workspaces SET volume_id = '', state = $1, last_error = $2
         WHERE id = $3 AND state = $4",
    )
    .bind(RESET_FAILED)
    .bind(error)
    .bind(id)
    .bind(RESETTING)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_error(pool: &AnyPool, id: &str, error: &str) -> Result<()> {
    sqlx::query("UPDATE agent_workspaces SET last_error = $1 WHERE id = $2")
        .bind(error)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM agent_workspaces WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
