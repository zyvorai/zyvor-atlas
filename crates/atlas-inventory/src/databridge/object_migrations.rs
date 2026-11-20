// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Object-storage migration records (cloud object store -> Ceph RGW). Mirrors the
//! `plans.rs` insert/set/get/list pattern. Credentials are never stored — only secret refs.

use anyhow::Result;
use atlas_api_types::ObjectMigration;
use sqlx::{AnyPool, Row};

/// Fields the caller supplies at create time. Grouped to avoid a 15-argument function.
#[derive(Debug, Clone)]
pub struct NewObjectMigration {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub source_provider: String,
    pub source_endpoint: String,
    pub source_region: String,
    pub source_bucket: String,
    pub source_prefix: Option<String>,
    pub source_secret_ref: Option<String>,
    pub dest_provider: String,
    pub dest_endpoint: String,
    pub dest_region: String,
    pub dest_bucket: String,
    pub dest_secret_ref: Option<String>,
    pub secret_namespace: String,
    pub mode: String,
    pub concurrency: Option<i64>,
    pub part_size_mb: Option<i64>,
}

pub async fn insert(pool: &AnyPool, m: &NewObjectMigration) -> Result<()> {
    sqlx::query(
        "INSERT INTO object_migrations
         (id, tenant_id, name, source_provider, source_endpoint, source_region, source_bucket,
          source_prefix, source_secret_ref, dest_provider, dest_endpoint, dest_region,
          dest_bucket, dest_secret_ref, secret_namespace, mode, concurrency, part_size_mb, state)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, 'created')",
    )
    .bind(&m.id)
    .bind(&m.tenant_id)
    .bind(&m.name)
    .bind(&m.source_provider)
    .bind(&m.source_endpoint)
    .bind(&m.source_region)
    .bind(&m.source_bucket)
    .bind(&m.source_prefix)
    .bind(&m.source_secret_ref)
    .bind(&m.dest_provider)
    .bind(&m.dest_endpoint)
    .bind(&m.dest_region)
    .bind(&m.dest_bucket)
    .bind(&m.dest_secret_ref)
    .bind(&m.secret_namespace)
    .bind(&m.mode)
    .bind(m.concurrency)
    .bind(m.part_size_mb)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark the copy as started (used to compute throughput).
pub async fn set_started(pool: &AnyPool, id: &str) -> Result<()> {
    let now = crate::now_rfc3339(chrono::Utc::now());
    sqlx::query(
        "UPDATE object_migrations SET started_at=$1,
         updated_at=$2 WHERE id=$3 AND started_at IS NULL",
    )
    .bind(now.clone())
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_state(pool: &AnyPool, id: &str, state: &str) -> Result<()> {
    sqlx::query("UPDATE object_migrations SET state=$1, updated_at=$2 WHERE id=$3")
        .bind(state)
        .bind(crate::now_rfc3339(chrono::Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_job(pool: &AnyPool, id: &str, job_id: &str) -> Result<()> {
    sqlx::query("UPDATE object_migrations SET job_id=$1, updated_at=$2 WHERE id=$3")
        .bind(job_id)
        .bind(crate::now_rfc3339(chrono::Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_totals(pool: &AnyPool, id: &str, objects: i64, bytes: i64) -> Result<()> {
    sqlx::query(
        "UPDATE object_migrations SET objects_total=$1, bytes_total=$2,
         updated_at=$3 WHERE id=$4",
    )
    .bind(objects)
    .bind(bytes)
    .bind(crate::now_rfc3339(chrono::Utc::now()))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_progress(
    pool: &AnyPool,
    id: &str,
    objects_done: i64,
    bytes_done: i64,
    throughput_mbps: f64,
) -> Result<()> {
    sqlx::query(
        "UPDATE object_migrations SET objects_done=$1, bytes_done=$2, throughput_mbps=$3,
         updated_at=$4 WHERE id=$5",
    )
    .bind(objects_done)
    .bind(bytes_done)
    .bind(throughput_mbps)
    .bind(crate::now_rfc3339(chrono::Utc::now()))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Terminal outcome: verified + final state, clearing/setting the error.
pub async fn finish(
    pool: &AnyPool,
    id: &str,
    state: &str,
    verified: bool,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE object_migrations SET state=$1, verified=$2, last_error=$3,
         updated_at=$4 WHERE id=$5",
    )
    .bind(state)
    .bind(if verified { 1 } else { 0 })
    .bind(error)
    .bind(crate::now_rfc3339(chrono::Utc::now()))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get(pool: &AnyPool, id: &str) -> Result<Option<ObjectMigration>> {
    let row = sqlx::query(select("WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to))
}

pub async fn list(pool: &AnyPool, tenant_id: Option<&str>) -> Result<Vec<ObjectMigration>> {
    let rows = match tenant_id {
        Some(t) => {
            sqlx::query(select("WHERE tenant_id = $1 ORDER BY created_at DESC"))
                .bind(t)
                .fetch_all(pool)
                .await?
        }
        None => {
            sqlx::query(select("ORDER BY created_at DESC"))
                .fetch_all(pool)
                .await?
        }
    };
    Ok(rows.into_iter().map(row_to).collect())
}

pub async fn delete(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM object_migrations WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// `tail` is only ever a string literal from this module's own call sites (never user input — user
// data always goes through `$N` bind parameters), so asserting the assembled SQL safe is sound.
fn select(tail: &str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT id, tenant_id, name, source_provider, source_endpoint, source_region,
                source_bucket, source_prefix, source_secret_ref, dest_provider, dest_endpoint,
                dest_region, dest_bucket, dest_secret_ref, secret_namespace, mode, state,
                objects_total, objects_done, bytes_total, bytes_done, verified,
                concurrency, part_size_mb, throughput_mbps, started_at, last_error,
                job_id, created_at, updated_at
         FROM object_migrations {tail}"
    ))
}

fn row_to(r: sqlx::any::AnyRow) -> ObjectMigration {
    let verified: i64 = r.get("verified");
    ObjectMigration {
        id: r.get("id"),
        tenant_id: r.get("tenant_id"),
        name: r.get("name"),
        source_provider: r.get("source_provider"),
        source_endpoint: r.get("source_endpoint"),
        source_region: r.get("source_region"),
        source_bucket: r.get("source_bucket"),
        source_prefix: r.get("source_prefix"),
        source_secret_ref: r.get("source_secret_ref"),
        dest_provider: r.get("dest_provider"),
        dest_endpoint: r.get("dest_endpoint"),
        dest_region: r.get("dest_region"),
        dest_bucket: r.get("dest_bucket"),
        dest_secret_ref: r.get("dest_secret_ref"),
        secret_namespace: r.get("secret_namespace"),
        mode: r.get("mode"),
        state: r.get("state"),
        objects_total: r.get("objects_total"),
        objects_done: r.get("objects_done"),
        bytes_total: r.get("bytes_total"),
        bytes_done: r.get("bytes_done"),
        verified: verified != 0,
        concurrency: r.get("concurrency"),
        part_size_mb: r.get("part_size_mb"),
        throughput_mbps: r.get("throughput_mbps"),
        started_at: r.get("started_at"),
        last_error: r.get("last_error"),
        job_id: r.get("job_id"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}
