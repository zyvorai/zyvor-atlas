// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Backup records + manifests (PDF §16.2, §11 storage_backups).

use anyhow::Result;
use atlas_api_types::BackupRecord;
use sqlx::{AnyPool, Row};

#[allow(clippy::too_many_arguments)]
pub async fn insert_backup(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    volume_id: &str,
    snapshot_id: Option<&str>,
    bucket_id: &str,
    object_key: &str,
    format: &str,
    manifest: &serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO storage_backups (id, tenant_id, volume_id, snapshot_id, bucket_id, object_key, format, state, manifest)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending', $8)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(volume_id)
    .bind(snapshot_id)
    .bind(bucket_id)
    .bind(object_key)
    .bind(format)
    .bind(manifest.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_state(
    pool: &AnyPool,
    id: &str,
    state: &str,
    checksum: Option<&str>,
    format: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE storage_backups SET state=$1, checksum=COALESCE($2, checksum), format=COALESCE($3, format) WHERE id=$4",
    )
    .bind(state)
    .bind(checksum)
    .bind(format)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_backup_row(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM storage_backups WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Count backups referencing a bucket (blocks bucket deletion while non-empty).
pub async fn count_for_bucket(pool: &AnyPool, bucket_id: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM storage_backups WHERE bucket_id=$1")
            .bind(bucket_id)
            .fetch_one(pool)
            .await?,
    )
}

pub async fn get_backup(pool: &AnyPool, id: &str) -> Result<Option<BackupRecord>> {
    let row = sqlx::query(select("WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_backup))
}

pub async fn list_backups(pool: &AnyPool, volume_id: Option<&str>) -> Result<Vec<BackupRecord>> {
    let rows = match volume_id {
        Some(v) => {
            sqlx::query(select("WHERE volume_id = $1 ORDER BY created_at DESC"))
                .bind(v)
                .fetch_all(pool)
                .await?
        }
        None => {
            sqlx::query(select("ORDER BY created_at DESC"))
                .fetch_all(pool)
                .await?
        }
    };
    Ok(rows.into_iter().map(row_to_backup).collect())
}

/// Backups whose source volume no longer exists — orphans. `storage_backups.volume_id` has no FK
/// (unlike snapshots, which cascade), so deleting a volume leaves its backups dangling in the
/// catalog. Day-2 hygiene surfaces these so an operator can reclaim them.
pub async fn list_orphans(pool: &AnyPool) -> Result<Vec<BackupRecord>> {
    let rows = sqlx::query(select(
        "WHERE NOT EXISTS (SELECT 1 FROM storage_volumes v WHERE v.id = storage_backups.volume_id) \
         ORDER BY created_at DESC",
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_backup).collect())
}

/// List completed backups (`verified`/`completed`) for a volume created strictly before `cutoff`
/// (RFC3339 UTC, DB format `YYYY-MM-DDTHH:MM:SS.mmmZ`), oldest first — used by age-based retention.
pub async fn list_older_than(
    pool: &AnyPool,
    volume_id: &str,
    cutoff: &str,
) -> Result<Vec<BackupRecord>> {
    let rows = sqlx::query(select(
        "WHERE volume_id = $1 AND state IN ('verified', 'completed') AND created_at < $2 \
         ORDER BY created_at ASC",
    ))
    .bind(volume_id)
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_backup).collect())
}

/// The most recent backup per volume, in one query — used by `protection::list_protection_status`
/// so a bank-scale fleet doesn't pay one query per volume.
pub async fn latest_by_volume(
    pool: &AnyPool,
) -> Result<std::collections::HashMap<String, BackupRecord>> {
    let rows = sqlx::query(
        "SELECT b.id, b.tenant_id, b.volume_id, b.snapshot_id, b.bucket_id, b.object_key, \
                b.format, b.checksum, b.state, b.created_at \
         FROM storage_backups b \
         INNER JOIN (SELECT volume_id, MAX(created_at) AS max_created FROM storage_backups GROUP BY volume_id) latest \
           ON latest.volume_id = b.volume_id AND latest.max_created = b.created_at",
    )
    .fetch_all(pool)
    .await?;
    let mut out = std::collections::HashMap::new();
    for r in rows {
        let backup = row_to_backup(r);
        out.entry(backup.volume_id.clone()).or_insert(backup);
    }
    Ok(out)
}

// `tail` is only ever a string literal from this module's own call sites (never user input — user
// data always goes through `$N` bind parameters), so asserting the assembled SQL safe is sound.
fn select(tail: &str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT id, tenant_id, volume_id, snapshot_id, bucket_id, object_key, format, checksum, state, created_at
         FROM storage_backups {tail}"
    ))
}

fn row_to_backup(r: sqlx::any::AnyRow) -> BackupRecord {
    BackupRecord {
        id: r.get("id"),
        tenant_id: r.get("tenant_id"),
        volume_id: r.get("volume_id"),
        snapshot_id: r.get("snapshot_id"),
        bucket_id: r.get("bucket_id"),
        object_key: r.get("object_key"),
        format: r.get("format"),
        checksum: r.get("checksum"),
        state: r.get("state"),
        created_at: r.get("created_at"),
    }
}
