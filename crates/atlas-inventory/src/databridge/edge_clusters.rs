// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Edge database cluster records (CloudNativePG / MySQL operator on Ceph).

use anyhow::Result;
use atlas_api_types::EdgeDbCluster;
use sqlx::{AnyPool, Row};

#[allow(clippy::too_many_arguments)]
pub async fn insert_edge_cluster(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    plan_id: &str,
    engine: &str,
    operator: &str,
    namespace: &str,
    cr_name: &str,
    storage_class: &str,
    wal_storage_class: Option<&str>,
    instances: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO edge_db_clusters
         (id, tenant_id, plan_id, engine, operator, namespace, cr_name, storage_class,
          wal_storage_class, instances, state)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'provisioning')",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(plan_id)
    .bind(engine)
    .bind(operator)
    .bind(namespace)
    .bind(cr_name)
    .bind(storage_class)
    .bind(wal_storage_class)
    .bind(instances)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark the cluster ready and record its service endpoint + credentials Secret.
pub async fn set_ready(
    pool: &AnyPool,
    id: &str,
    service_endpoint: &str,
    secret_ref: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE edge_db_clusters SET service_endpoint=$1, secret_ref=$2, state='ready' WHERE id=$3",
    )
    .bind(service_endpoint)
    .bind(secret_ref)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_state(pool: &AnyPool, id: &str, state: &str) -> Result<()> {
    sqlx::query("UPDATE edge_db_clusters SET state=$1 WHERE id=$2")
        .bind(state)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record the data volume actually loaded onto the edge cluster (the column otherwise stays stuck
/// at its SQL default of 0 forever — nothing else ever writes it).
pub async fn set_size_bytes(pool: &AnyPool, id: &str, size_bytes: i64) -> Result<()> {
    sqlx::query("UPDATE edge_db_clusters SET size_bytes=$1 WHERE id=$2")
        .bind(size_bytes)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete an edge cluster inventory row (e.g. an orphaned/stale cluster). The backing operator CR
/// is torn down separately; this only removes the control-plane record.
pub async fn delete_edge_cluster(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM edge_db_clusters WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_edge_cluster(pool: &AnyPool, id: &str) -> Result<Option<EdgeDbCluster>> {
    let row = sqlx::query(select("WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_edge))
}

pub async fn list_edge_clusters(pool: &AnyPool) -> Result<Vec<EdgeDbCluster>> {
    let rows = sqlx::query(select("ORDER BY created_at DESC"))
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row_to_edge).collect())
}

/// Clusters in a given state — used by the reconciler to poll provisioning ones.
pub async fn list_by_state(pool: &AnyPool, state: &str) -> Result<Vec<EdgeDbCluster>> {
    let rows = sqlx::query(select("WHERE state = $1 ORDER BY created_at DESC"))
        .bind(state)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row_to_edge).collect())
}

// `tail` is only ever a string literal from this module's own call sites (never user input — user
// data always goes through `$N` bind parameters), so asserting the assembled SQL safe is sound.
fn select(tail: &str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT id, tenant_id, plan_id, engine, operator, namespace, cr_name, storage_class,
                wal_storage_class, instances, size_bytes, service_endpoint, secret_ref, state, created_at
         FROM edge_db_clusters {tail}"
    ))
}

fn row_to_edge(r: sqlx::any::AnyRow) -> EdgeDbCluster {
    EdgeDbCluster {
        id: r.get("id"),
        tenant_id: r.get("tenant_id"),
        plan_id: r.get("plan_id"),
        engine: r.get("engine"),
        operator: r.get("operator"),
        namespace: r.get("namespace"),
        cr_name: r.get("cr_name"),
        storage_class: r.get("storage_class"),
        wal_storage_class: r.get("wal_storage_class"),
        instances: r.get("instances"),
        size_bytes: r.get("size_bytes"),
        service_endpoint: r.get("service_endpoint"),
        secret_ref: r.get("secret_ref"),
        state: r.get("state"),
        created_at: r.get("created_at"),
    }
}
