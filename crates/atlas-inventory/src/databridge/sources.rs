// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Migration source records (registered cloud/source databases).

use anyhow::Result;
use atlas_api_types::MigrationSource;
use sqlx::{AnyPool, Row};

/// Insert a registered source (before discovery runs).
#[allow(clippy::too_many_arguments)]
pub async fn insert_source(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    name: &str,
    kind: &str,
    cloud: &str,
    endpoint: Option<&str>,
    port: Option<i64>,
    database: Option<&str>,
    secret_ref: Option<&str>,
    secret_namespace: Option<&str>,
    tls_mode: &str,
    driver_mode: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO migration_sources
         (id, tenant_id, name, kind, cloud, endpoint, port, database, secret_ref,
          secret_namespace, tls_mode, driver_mode, state)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'registered')",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(name)
    .bind(kind)
    .bind(cloud)
    .bind(endpoint)
    .bind(port)
    .bind(database)
    .bind(secret_ref)
    .bind(secret_namespace)
    .bind(tls_mode)
    .bind(driver_mode)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a discovery result and flip state to `discovered`.
pub async fn set_discovered(
    pool: &AnyPool,
    id: &str,
    discovered: &serde_json::Value,
) -> Result<()> {
    sqlx::query("UPDATE migration_sources SET discovered=$1, state='discovered' WHERE id=$2")
        .bind(discovered.to_string())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_state(pool: &AnyPool, id: &str, state: &str) -> Result<()> {
    sqlx::query("UPDATE migration_sources SET state=$1 WHERE id=$2")
        .bind(state)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_source_row(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM migration_sources WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_source(pool: &AnyPool, id: &str) -> Result<Option<MigrationSource>> {
    let row = sqlx::query(select("WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_source))
}

pub async fn list_sources(pool: &AnyPool) -> Result<Vec<MigrationSource>> {
    let rows = sqlx::query(select("ORDER BY created_at DESC"))
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row_to_source).collect())
}

// `tail` is only ever a string literal from this module's own call sites (never user input — user
// data always goes through `$N` bind parameters), so asserting the assembled SQL safe is sound.
fn select(tail: &str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT id, tenant_id, name, kind, cloud, endpoint, port, database, secret_ref,
                secret_namespace, tls_mode, driver_mode, state, discovered, created_at
         FROM migration_sources {tail}"
    ))
}

fn row_to_source(r: sqlx::any::AnyRow) -> MigrationSource {
    let discovered: String = r.get("discovered");
    MigrationSource {
        id: r.get("id"),
        tenant_id: r.get("tenant_id"),
        name: r.get("name"),
        kind: r.get("kind"),
        cloud: r.get("cloud"),
        endpoint: r.get("endpoint"),
        port: r.get("port"),
        database: r.get("database"),
        secret_ref: r.get("secret_ref"),
        secret_namespace: r.get("secret_namespace"),
        tls_mode: r.get("tls_mode"),
        driver_mode: r.get("driver_mode"),
        state: r.get("state"),
        discovered: serde_json::from_str(&discovered).unwrap_or(serde_json::Value::Null),
        created_at: r.get("created_at"),
    }
}
