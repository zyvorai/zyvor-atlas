// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Multi-image consistency groups: the catalog side of `rbd group` (members and group snapshots).
//! The gateway runs the live `rbd group` commands; this records what they did.

use anyhow::Result;
use sqlx::{AnyPool, Row};

/// One group member: `(volume_id, pool, image)`.
pub type Member = (String, String, String);

pub async fn create(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    name: &str,
    rbd_pool: &str,
    members: &[Member],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO volume_groups (id, tenant_id, name, pool) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(tenant_id)
        .bind(name)
        .bind(rbd_pool)
        .execute(&mut *tx)
        .await?;
    for (volume_id, mpool, image) in members {
        sqlx::query(
            "INSERT INTO volume_group_members (group_id, volume_id, pool, image)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(volume_id)
        .bind(mpool)
        .bind(image)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn exists(pool: &AnyPool, rbd_pool: &str, name: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM volume_groups WHERE pool=$1 AND name=$2")
        .bind(rbd_pool)
        .bind(name)
        .fetch_one(pool)
        .await?;
    Ok(n > 0)
}

/// `(name, pool)` of a group.
pub async fn get(pool: &AnyPool, id: &str) -> Result<Option<(String, String)>> {
    let row = sqlx::query("SELECT name, pool FROM volume_groups WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| (r.get("name"), r.get("pool"))))
}

pub async fn members(pool: &AnyPool, id: &str) -> Result<Vec<Member>> {
    let rows = sqlx::query(
        "SELECT volume_id, pool, image FROM volume_group_members WHERE group_id=$1
         ORDER BY volume_id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("volume_id"), r.get("pool"), r.get("image")))
        .collect())
}

/// The group a volume already belongs to, if any (an RBD image can be in at most one group).
pub async fn group_of_volume(pool: &AnyPool, volume_id: &str) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT group_id FROM volume_group_members WHERE volume_id=$1")
            .bind(volume_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn list(pool: &AnyPool) -> Result<Vec<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT g.id, g.tenant_id, g.name, g.pool, g.created_at,
                (SELECT COUNT(*) FROM volume_group_members m WHERE m.group_id = g.id) AS members,
                (SELECT COUNT(*) FROM volume_group_snapshots s WHERE s.group_id = g.id) AS snapshots
         FROM volume_groups g ORDER BY g.created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.get::<String, _>("id"),
                "tenant_id": r.get::<String, _>("tenant_id"),
                "name": r.get::<String, _>("name"),
                "pool": r.get::<String, _>("pool"),
                "members": r.get::<i64, _>("members"),
                "snapshots": r.get::<i64, _>("snapshots"),
                "created_at": r.get::<String, _>("created_at"),
            })
        })
        .collect())
}

pub async fn delete(pool: &AnyPool, id: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    for sql in [
        "DELETE FROM volume_group_snapshots WHERE group_id=$1",
        "DELETE FROM volume_group_members WHERE group_id=$1",
        "DELETE FROM volume_groups WHERE id=$1",
    ] {
        sqlx::query(sql).bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn add_snapshot(pool: &AnyPool, id: &str, group_id: &str, name: &str) -> Result<()> {
    sqlx::query("INSERT INTO volume_group_snapshots (id, group_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(group_id)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn snapshot_exists(pool: &AnyPool, group_id: &str, name: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM volume_group_snapshots WHERE group_id=$1 AND name=$2",
    )
    .bind(group_id)
    .bind(name)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

pub async fn snapshots(pool: &AnyPool, group_id: &str) -> Result<Vec<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT id, name, created_at FROM volume_group_snapshots WHERE group_id=$1
         ORDER BY created_at DESC",
    )
    .bind(group_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.get::<String, _>("id"),
                "name": r.get::<String, _>("name"),
                "created_at": r.get::<String, _>("created_at"),
            })
        })
        .collect())
}

pub async fn delete_snapshot(pool: &AnyPool, group_id: &str, name: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM volume_group_snapshots WHERE group_id=$1 AND name=$2")
        .bind(group_id)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}
