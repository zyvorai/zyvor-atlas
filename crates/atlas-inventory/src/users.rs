// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Console users for username/password sign-in (admin-managed, role = privilege level).

use anyhow::{bail, Result};
use serde::Serialize;
use sqlx::{AnyPool, Row};

use crate::now_rfc3339;

#[derive(Debug, Clone, Serialize)]
pub struct ConsoleUser {
    pub username: String,
    pub role: String,
    /// Tenant this user is scoped to on read endpoints (ignored for `role = "admin"`, which stays
    /// cross-tenant regardless of this value). Defaults to `"global"`.
    pub tenant_id: String,
    pub disabled: bool,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn map_row(r: sqlx::any::AnyRow) -> ConsoleUser {
    ConsoleUser {
        username: r.get("username"),
        role: r.get("role"),
        tenant_id: r.get("tenant_id"),
        disabled: r.get::<i64, _>("disabled") != 0,
        created_by: r.get("created_by"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

pub async fn list(pool: &AnyPool) -> Result<Vec<ConsoleUser>> {
    // `lower(username)`, not `COLLATE NOCASE` (SQLite-only) — portable on both backends. Both
    // migrations/ and migrations-postgres/ carry a matching `lower(username)` expression index.
    let rows = sqlx::query(
        "SELECT username, role, tenant_id, disabled, created_by, created_at, updated_at \
         FROM console_users ORDER BY lower(username)",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(map_row).collect())
}

pub async fn get(pool: &AnyPool, username: &str) -> Result<Option<ConsoleUser>> {
    let row = sqlx::query(
        "SELECT username, role, tenant_id, disabled, created_by, created_at, updated_at \
         FROM console_users WHERE lower(username) = lower($1)",
    )
    .bind(username)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(map_row))
}

/// Fetch password hash + role + tenant_id for login. Returns None when the user is missing or
/// disabled.
pub async fn credentials_for_login(
    pool: &AnyPool,
    username: &str,
) -> Result<Option<(String, String, String)>> {
    let row = sqlx::query(
        "SELECT password_hash, role, tenant_id, disabled FROM console_users WHERE lower(username) = lower($1)",
    )
    .bind(username)
    .fetch_optional(pool)
    .await?;
    let Some(r) = row else {
        return Ok(None);
    };
    if r.get::<i64, _>("disabled") != 0 {
        return Ok(None);
    }
    Ok(Some((
        r.get("password_hash"),
        r.get("role"),
        r.get("tenant_id"),
    )))
}

pub async fn create(
    pool: &AnyPool,
    username: &str,
    password_hash: &str,
    role: &str,
    tenant_id: &str,
    created_by: &str,
) -> Result<ConsoleUser> {
    let res = sqlx::query(
        "INSERT INTO console_users (username, password_hash, role, tenant_id, created_by) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(username)
    .bind(password_hash)
    .bind(role)
    .bind(tenant_id)
    .bind(created_by)
    .execute(pool)
    .await;
    match res {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            bail!("user '{username}' already exists");
        }
        Err(e) => return Err(e.into()),
    }
    get(pool, username)
        .await?
        .ok_or_else(|| anyhow::anyhow!("user created but not found"))
}

pub async fn update(
    pool: &AnyPool,
    username: &str,
    role: Option<&str>,
    password_hash: Option<&str>,
    disabled: Option<bool>,
) -> Result<Option<ConsoleUser>> {
    if role.is_none() && password_hash.is_none() && disabled.is_none() {
        return get(pool, username).await;
    }
    let existing = get(pool, username).await?;
    if existing.is_none() {
        return Ok(None);
    }
    sqlx::query(
        "UPDATE console_users SET \
            role = COALESCE($1, role), \
            password_hash = COALESCE($2, password_hash), \
            disabled = COALESCE($3, disabled), \
            updated_at = $4 \
         WHERE lower(username) = lower($5)",
    )
    .bind(role)
    .bind(password_hash)
    .bind(disabled.map(|d| if d { 1i64 } else { 0 }))
    .bind(now_rfc3339(chrono::Utc::now()))
    .bind(username)
    .execute(pool)
    .await?;
    get(pool, username).await
}

pub async fn delete(pool: &AnyPool, username: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM console_users WHERE lower(username) = lower($1)")
        .bind(username)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn count_admins(pool: &AnyPool) -> Result<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM console_users WHERE role = 'admin' AND disabled = 0",
    )
    .fetch_one(pool)
    .await?;
    Ok(n)
}
