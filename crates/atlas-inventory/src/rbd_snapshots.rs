// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Fake-driver-mode catalog for direct RBD image snapshots. Real mode reads snapshots straight
//! from Ceph (`rbd snap ls`) and never touches this table.

use anyhow::Result;
use sqlx::{AnyPool, Row};

pub async fn create(pool: &AnyPool, rbd_pool: &str, image: &str, snap: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO rbd_snapshots (pool, image, snap) VALUES ($1, $2, $3)
         ON CONFLICT(pool, image, snap) DO NOTHING",
    )
    .bind(rbd_pool)
    .bind(image)
    .bind(snap)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete(pool: &AnyPool, rbd_pool: &str, image: &str, snap: &str) -> Result<()> {
    sqlx::query("DELETE FROM rbd_snapshots WHERE pool=$1 AND image=$2 AND snap=$3")
        .bind(rbd_pool)
        .bind(image)
        .bind(snap)
        .execute(pool)
        .await?;
    Ok(())
}

/// Drop every snapshot recorded for an image — called when the image itself is deleted, so a
/// same-named image created later doesn't inherit stale snapshot rows.
pub async fn delete_all_for_image(pool: &AnyPool, rbd_pool: &str, image: &str) -> Result<()> {
    sqlx::query("DELETE FROM rbd_snapshots WHERE pool=$1 AND image=$2")
        .bind(rbd_pool)
        .bind(image)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list(pool: &AnyPool, rbd_pool: &str, image: &str) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT snap FROM rbd_snapshots WHERE pool=$1 AND image=$2 ORDER BY created_at",
    )
    .bind(rbd_pool)
    .bind(image)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.get("snap")).collect())
}
