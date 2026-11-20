// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Per-tenant storage quotas + live usage (PDF §14 multi-tenancy). Usage is computed from the live
//! `storage_volumes` rows for the tenant, so it always reflects what is currently provisioned.

use anyhow::Result;
use atlas_api_types::{TenantPolicy, TenantQuota};
use sqlx::{AnyPool, Row};

use crate::now_rfc3339;

/// Overview of every tenant that owns volumes or has a quota: usage + limits, one row each.
pub async fn list_overview(pool: &AnyPool) -> Result<Vec<TenantQuota>> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT tenant_id FROM storage_volumes
         UNION SELECT tenant_id FROM storage_tenant_quotas
         ORDER BY tenant_id",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(get_quota(pool, &id).await?);
    }
    Ok(out)
}

/// Current usage for a tenant: total provisioned volume bytes and volume count.
pub async fn usage(pool: &AnyPool, tenant_id: &str) -> Result<(i64, i64)> {
    let row = sqlx::query(
        // CAST(... AS BIGINT), not bare SUM/COALESCE: Postgres's SUM(bigint) returns NUMERIC
        // (to avoid silent overflow), which sqlx's Any driver can't decode at all — the cast
        // forces a plain bigint result, portable to SQLite (whose CAST AS BIGINT gets INTEGER
        // affinity, identical to an uncast integer).
        "SELECT CAST(COALESCE(SUM(size_bytes), 0) AS BIGINT) AS used, COUNT(*) AS n
         FROM storage_volumes WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await?;
    Ok((row.get::<i64, _>("used"), row.get::<i64, _>("n")))
}

/// Fetch a tenant's quota (with current usage). Returns unlimited (0/0) limits if none is set.
pub async fn get_quota(pool: &AnyPool, tenant_id: &str) -> Result<TenantQuota> {
    let row = sqlx::query(
        "SELECT max_bytes, max_volumes FROM storage_tenant_quotas WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await?;
    let (max_bytes, max_volumes) = match row {
        Some(r) => (r.get::<i64, _>("max_bytes"), r.get::<i64, _>("max_volumes")),
        None => (0, 0),
    };
    let (used_bytes, volume_count) = usage(pool, tenant_id).await?;
    Ok(TenantQuota {
        tenant_id: tenant_id.to_string(),
        max_bytes,
        max_volumes,
        used_bytes,
        volume_count,
    })
}

/// Set (upsert) a tenant's quota. `0` for a limit means unlimited.
pub async fn set_quota(
    pool: &AnyPool,
    tenant_id: &str,
    max_bytes: i64,
    max_volumes: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO storage_tenant_quotas (tenant_id, max_bytes, max_volumes)
         VALUES ($1, $2, $3)
         ON CONFLICT(tenant_id) DO UPDATE SET
            max_bytes = excluded.max_bytes,
            max_volumes = excluded.max_volumes,
            updated_at = $4",
    )
    .bind(tenant_id)
    .bind(max_bytes)
    .bind(max_volumes)
    .bind(now_rfc3339(chrono::Utc::now()))
    .execute(pool)
    .await?;
    Ok(())
}

fn row_to_policy(r: sqlx::any::AnyRow) -> TenantPolicy {
    TenantPolicy {
        tenant_id: r.get("tenant_id"),
        intent: r.get("intent"),
        storage_class: r.get("storage_class"),
        access_mode: r.get("access_mode"),
        volume_mode: r.get("volume_mode"),
    }
}

/// Fetch a tenant's override for one intent, if any.
pub async fn get_policy(
    pool: &AnyPool,
    tenant_id: &str,
    intent: &str,
) -> Result<Option<TenantPolicy>> {
    let row = sqlx::query(
        "SELECT tenant_id, intent, storage_class, access_mode, volume_mode
         FROM tenant_policies WHERE tenant_id = $1 AND intent = $2",
    )
    .bind(tenant_id)
    .bind(intent)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_policy))
}

/// List all of a tenant's policy overrides.
pub async fn list_policies(pool: &AnyPool, tenant_id: &str) -> Result<Vec<TenantPolicy>> {
    let rows = sqlx::query(
        "SELECT tenant_id, intent, storage_class, access_mode, volume_mode
         FROM tenant_policies WHERE tenant_id = $1 ORDER BY intent",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_policy).collect())
}

/// Upsert a tenant's override for an intent.
pub async fn set_policy(
    pool: &AnyPool,
    tenant_id: &str,
    intent: &str,
    storage_class: &str,
    access_mode: &str,
    volume_mode: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO tenant_policies (tenant_id, intent, storage_class, access_mode, volume_mode)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT(tenant_id, intent) DO UPDATE SET
            storage_class = excluded.storage_class,
            access_mode = excluded.access_mode,
            volume_mode = excluded.volume_mode,
            updated_at = $6",
    )
    .bind(tenant_id)
    .bind(intent)
    .bind(storage_class)
    .bind(access_mode)
    .bind(volume_mode)
    .bind(now_rfc3339(chrono::Utc::now()))
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete a tenant's override for an intent. Returns whether a row was removed.
pub async fn delete_policy(pool: &AnyPool, tenant_id: &str, intent: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM tenant_policies WHERE tenant_id = $1 AND intent = $2")
        .bind(tenant_id)
        .bind(intent)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Result of a quota admission check.
pub enum QuotaCheck {
    Ok,
    /// The additional bytes would exceed the tenant's byte quota (limit, would_be_used).
    Bytes {
        limit: i64,
        would_be: i64,
    },
    /// One more volume would exceed the tenant's volume-count quota (limit, current_count).
    Count {
        limit: i64,
        current: i64,
    },
}

/// Check whether creating one volume of `add_bytes` is within the tenant's quota.
pub async fn check_admission(
    pool: &AnyPool,
    tenant_id: &str,
    add_bytes: i64,
) -> Result<QuotaCheck> {
    let q = get_quota(pool, tenant_id).await?;
    // `saturating_add`: a caller-supplied `add_bytes` near `i64::MAX` (e.g. a malformed/adversarial
    // `size_bytes`) would otherwise overflow this addition — panicking the request in a debug build,
    // or silently wrapping to a bogus (possibly negative) `would_be` that bypasses the quota check
    // in release.
    let would_be = q.used_bytes.saturating_add(add_bytes);
    if q.max_bytes > 0 && would_be > q.max_bytes {
        return Ok(QuotaCheck::Bytes {
            limit: q.max_bytes,
            would_be,
        });
    }
    if q.max_volumes > 0 && q.volume_count + 1 > q.max_volumes {
        return Ok(QuotaCheck::Count {
            limit: q.max_volumes,
            current: q.volume_count,
        });
    }
    Ok(QuotaCheck::Ok)
}
