// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Object-storage bucket records (PDF §9.9, §11 storage_buckets).

use anyhow::Result;
use atlas_api_types::StorageBucket;
use sqlx::{AnyPool, Row};

/// Insert a pending bucket row (after the OBC create call has been accepted by k8s, before it's
/// necessarily bound). The bare `ON CONFLICT DO NOTHING` makes this safe to call again on a job
/// retry that re-enters the same `BucketCreate` dispatch arm with the same `id` — the row from the
/// first attempt is left alone rather than erroring on the primary-key conflict.
#[allow(clippy::too_many_arguments)]
pub async fn insert_bucket(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    name: &str,
    backend_id: &str,
    namespace: &str,
    obc_name: &str,
    storage_class: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO storage_buckets (id, tenant_id, name, backend_id, namespace, obc_name, storage_class, state)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending')
         ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(name)
    .bind(backend_id)
    .bind(namespace)
    .bind(obc_name)
    .bind(storage_class)
    .execute(pool)
    .await?;
    Ok(())
}

/// Count buckets still provisioned against a StorageClass — the dependent-guard behind a Rook
/// `CephObjectStore` delete (`DELETE /ceph/object-stores/{name}`), mirroring
/// `count_volumes_by_storage_class`'s role in the pool/filesystem delete guards.
pub async fn count_by_storage_class(pool: &AnyPool, storage_class: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM storage_buckets WHERE storage_class = $1")
            .bind(storage_class)
            .fetch_one(pool)
            .await?,
    )
}

/// Fill in the bucket details once the OBC is bound.
#[allow(clippy::too_many_arguments)]
pub async fn set_bound(
    pool: &AnyPool,
    id: &str,
    bucket_name: &str,
    endpoint: &str,
    region: &str,
    secret_ref: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE storage_buckets SET bucket_name=$1, endpoint=$2, region=$3, secret_ref=$4, state='bound' WHERE id=$5",
    )
    .bind(bucket_name)
    .bind(endpoint)
    .bind(region)
    .bind(secret_ref)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_bucket_row(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM storage_buckets WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_bucket(pool: &AnyPool, id: &str) -> Result<Option<StorageBucket>> {
    let row = sqlx::query(select("WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_bucket))
}

pub async fn list_buckets(pool: &AnyPool) -> Result<Vec<StorageBucket>> {
    let rows = sqlx::query(select("ORDER BY created_at DESC"))
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row_to_bucket).collect())
}

// `tail` is only ever a string literal from this module's own call sites (never user input — user
// data always goes through `$N` bind parameters), so asserting the assembled SQL safe is sound.
fn select(tail: &str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT id, tenant_id, name, backend_id, bucket_name, endpoint, region, secret_ref, namespace, state, created_at
         FROM storage_buckets {tail}"
    ))
}

fn row_to_bucket(r: sqlx::any::AnyRow) -> StorageBucket {
    StorageBucket {
        id: r.get("id"),
        tenant_id: r.get("tenant_id"),
        name: r.get("name"),
        backend_id: r.get("backend_id"),
        bucket_name: r.get("bucket_name"),
        endpoint: r.get("endpoint"),
        region: r.get("region"),
        secret_ref: r.get("secret_ref"),
        namespace: r.get("namespace"),
        state: r.get("state"),
        created_at: r.get("created_at"),
    }
}
