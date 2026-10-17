// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! SQLite-backed normalized inventory: connect/migrate, upsert-from-discovery, and read queries
//! that power the gateway's read-only endpoints.
//!
//! Connect/migrate mirrors `machina/controller/src/db/mod.rs` (WAL, `create_if_missing`,
//! `sqlx::migrate!`). Read queries return `atlas-api-types` DTOs.

use std::time::Duration;

use anyhow::Result;
use atlas_api_types::{
    BackendMode, BackendType, Capabilities, DiscoveryResult, Health, Osd, StorageBackend,
    StorageCluster, StorageHealth, StoragePool, StorageVolume, VolumeKind,
};
use sqlx::any::{install_default_drivers, AnyPoolOptions};
use sqlx::{AnyPool, Row};

pub mod alert_notifications;
pub mod alerts;
pub mod audit;
pub mod backups;
pub mod buckets;
pub mod databridge;
pub mod dr;
pub mod events;
pub mod jobs;
pub mod leader;
pub mod metrics;
pub mod protection;
pub mod rate_limit;
pub mod rbd_snapshots;
pub mod schedules;
pub mod snapshots;
pub mod tenants;
pub mod tokens;
pub mod users;
pub mod volume_groups;

/// True when `database_url` is a `postgres://`/`postgresql://` URL; false (SQLite) otherwise.
/// The one place backend detection happens — everything else (`connect`, `migrate`, the state
/// backup's `VACUUM INTO` vs `pg_dump` branch in `atlas-gateway`) asks this, not the URL directly.
pub fn is_postgres_url(database_url: &str) -> bool {
    let lower = database_url.to_ascii_lowercase();
    lower.starts_with("postgres://") || lower.starts_with("postgresql://")
}

/// Open a backend-agnostic pool (SQLite or Postgres, chosen by `database_url`'s scheme) via
/// sqlx's `Any` driver. Every query in this crate (and `atlas-gateway`/`atlas-jobs`/
/// `atlas-monitor`) now targets `AnyPool` with `$1`/`$2`-style placeholders, which both backends'
/// concrete drivers accept identically — see `docs/HA.md` for why this replaced the earlier
/// "dual query modules vs `sqlx::Any`" open question.
///
/// SQLite-only per-connection tuning (WAL, `foreign_keys`, `busy_timeout`, `synchronous`) can't
/// be set through `AnyConnectOptions` (it's an opaque URL wrapper — verified against sqlx 0.8.6's
/// source: no backend-specific builder methods exist on it), so it's applied via `after_connect`
/// as literal `PRAGMA` statements instead, run once per pooled connection the same way the old
/// `SqliteConnectOptions` builder calls would have — skipped entirely for Postgres, where these
/// statements aren't valid syntax.
pub async fn connect(database_url: &str) -> Result<AnyPool> {
    install_default_drivers();
    let is_postgres = is_postgres_url(database_url);
    let max_connections = if is_postgres { 10 } else { 3 }; // SQLite allows one writer; keep the pool small so discovery + jobs don't stampede.
    let pool = AnyPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(60))
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                if !is_postgres {
                    sqlx::query("PRAGMA journal_mode = WAL")
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query("PRAGMA foreign_keys = ON")
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query("PRAGMA busy_timeout = 60000")
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query("PRAGMA synchronous = NORMAL")
                        .execute(&mut *conn)
                        .await?;
                }
                Ok(())
            })
        })
        .connect(database_url)
        .await?;
    Ok(pool)
}

/// Run the embedded migrations — `migrations/` (SQLite dialect) or `migrations-postgres/`
/// (Postgres dialect), chosen by `database_url`'s scheme. The two directories are kept in lockstep
/// (`scripts/check-migrations-parity.sh`, wired into CI) — every SQLite migration has a Postgres
/// counterpart with the same number, translated per the mapping documented at the top of
/// `migrations/0001_init.sql`.
pub async fn migrate(pool: &AnyPool, database_url: &str) -> Result<()> {
    if is_postgres_url(database_url) {
        sqlx::migrate!("../../migrations-postgres")
            .run(pool)
            .await?;
    } else {
        sqlx::migrate!("../../migrations").run(pool).await?;
    }
    Ok(())
}

/// Format a UTC timestamp exactly the way `strftime('%Y-%m-%dT%H:%M:%fZ', ...)` did — RFC3339
/// with 3-decimal-place milliseconds, e.g. `2026-09-15T08:52:55.566Z`. Used to bind "now" (or a
/// relative offset computed via `chrono::Duration`) as a query parameter instead of computing it
/// server-side, which is what let ~80 `strftime('now', ...)` call sites become backend-agnostic —
/// see the migration plan / `docs/HA.md`.
pub fn now_rfc3339(t: chrono::DateTime<chrono::Utc>) -> String {
    // Verified byte-for-byte identical to SQLite's strftime('%Y-%m-%dT%H:%M:%fZ', ...) output
    // (live comparison) — %S%.3f, not %.3f alone, since chrono's %.3f is fractional-seconds-only.
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

// ---------------------------------------------------------------------------
// Enum <-> TEXT helpers
// ---------------------------------------------------------------------------

fn health_str(h: Health) -> &'static str {
    match h {
        Health::Ok => "ok",
        Health::Warn => "warn",
        Health::Critical => "critical",
        Health::Unknown => "unknown",
    }
}
fn health_from(s: &str) -> Health {
    match s {
        "ok" => Health::Ok,
        "warn" => Health::Warn,
        "critical" => Health::Critical,
        _ => Health::Unknown,
    }
}
fn backend_type_str(t: BackendType) -> &'static str {
    match t {
        BackendType::Ceph => "ceph",
        BackendType::Nfs => "nfs",
        BackendType::Zfs => "zfs",
        BackendType::Longhorn => "longhorn",
        BackendType::Rustfs => "rustfs",
        BackendType::San => "san",
        BackendType::CloudBlock => "cloud_block",
        BackendType::Kubernetes => "kubernetes",
        BackendType::Native => "native",
        BackendType::Weka => "weka",
        BackendType::Macos => "macos",
    }
}
fn backend_type_from(s: &str) -> BackendType {
    match s {
        "nfs" => BackendType::Nfs,
        "zfs" => BackendType::Zfs,
        "longhorn" => BackendType::Longhorn,
        "rustfs" => BackendType::Rustfs,
        "san" => BackendType::San,
        "cloud_block" => BackendType::CloudBlock,
        "kubernetes" => BackendType::Kubernetes,
        "native" => BackendType::Native,
        "weka" => BackendType::Weka,
        "macos" => BackendType::Macos,
        _ => BackendType::Ceph,
    }
}
fn mode_str(m: BackendMode) -> &'static str {
    match m {
        BackendMode::ManagedRook => "managed_rook",
        BackendMode::External => "external",
        BackendMode::ReadOnly => "read_only",
    }
}
fn mode_from(s: &str) -> BackendMode {
    match s {
        "managed_rook" => BackendMode::ManagedRook,
        "read_only" => BackendMode::ReadOnly,
        _ => BackendMode::External,
    }
}
fn volume_kind_str(k: VolumeKind) -> &'static str {
    match k {
        VolumeKind::Block => "block",
        VolumeKind::Filesystem => "filesystem",
        VolumeKind::Object => "object",
    }
}
fn volume_kind_from(s: &str) -> VolumeKind {
    match s {
        "filesystem" => VolumeKind::Filesystem,
        "object" => VolumeKind::Object,
        _ => VolumeKind::Block,
    }
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Insert or update a backend registration row.
pub async fn upsert_backend(pool: &AnyPool, b: &StorageBackend) -> Result<()> {
    let caps = serde_json::to_string(&b.capabilities)?;
    sqlx::query(
        "INSERT INTO storage_backends (id, name, backend_type, mode, status, capabilities, connection_ref, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT(id) DO UPDATE SET
            name=excluded.name, backend_type=excluded.backend_type, mode=excluded.mode,
            status=excluded.status, capabilities=excluded.capabilities,
            connection_ref=excluded.connection_ref, updated_at=excluded.updated_at",
    )
    .bind(&b.id)
    .bind(&b.name)
    .bind(backend_type_str(b.backend_type))
    .bind(mode_str(b.mode))
    .bind(&b.status)
    .bind(caps)
    .bind(&b.connection_ref)
    .bind(now_rfc3339(chrono::Utc::now()))
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert or update a single volume (write path). Sets tenant/policy which discovery leaves default.
///
/// Races the periodic discovery worker: a `VolumeCreate` job only calls this once, at the very
/// end, after `poll_pvc_phase` has already waited for the PVC to reach `Bound` — by which point
/// the real RBD image is already visible to `rbd ls`. If a discovery tick fires in that window it
/// can insert its own row for the same `backend_native_id` first (under its driver-derived id,
/// via `upsert_discovery`'s own canonical-id resolution finding nothing yet to reconcile with).
/// This insert would then hit the partial UNIQUE index on `backend_native_id` (migration 0027)
/// and fail outright — worse, the caller's subsequent `product_bindings` insert uses `v.id`
/// regardless, which would silently reference a volume_id that was never actually created.
/// Verified live: `scripts/live/05-volume-lifecycle.sh`'s `POST /volumes` failed with
/// `UNIQUE constraint failed: storage_volumes.backend_native_id` under real concurrent discovery.
/// Reconcile by renaming the discovery-created row onto our intended id first, so the id the
/// caller is already waiting on (and about to bind product ownership to) is the one that ends up
/// live in inventory.
pub async fn upsert_volume(
    pool: &AnyPool,
    backend_id: &str,
    tenant_id: &str,
    v: &StorageVolume,
    policy_id: Option<&str>,
) -> Result<()> {
    if let Some(native) = v.backend_native_id.as_deref() {
        if let Some(existing_id) = sqlx::query_scalar::<_, String>(
            "SELECT id FROM storage_volumes WHERE backend_native_id = $1 AND id != $2",
        )
        .bind(native)
        .bind(&v.id)
        .fetch_optional(pool)
        .await?
        {
            rename_volume_id(pool, &existing_id, &v.id, native).await?;
        }
    }
    sqlx::query(
        "INSERT INTO storage_volumes
            (id, tenant_id, backend_id, cluster_id, pool_id, name, kind, backend_native_id, size_bytes, used_bytes,
             state, health, policy_id, kubernetes_namespace, pvc_name, storage_class_name, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
         ON CONFLICT(id) DO UPDATE SET
            tenant_id=excluded.tenant_id, cluster_id=excluded.cluster_id, pool_id=excluded.pool_id,
            name=excluded.name, kind=excluded.kind, backend_native_id=excluded.backend_native_id,
            size_bytes=excluded.size_bytes, used_bytes=excluded.used_bytes, state=excluded.state,
            health=excluded.health, policy_id=excluded.policy_id, kubernetes_namespace=excluded.kubernetes_namespace,
            pvc_name=excluded.pvc_name, storage_class_name=excluded.storage_class_name, updated_at=excluded.updated_at",
    )
    .bind(&v.id)
    .bind(tenant_id)
    .bind(backend_id)
    .bind(&v.cluster_id)
    .bind(&v.pool_id)
    .bind(&v.name)
    .bind(volume_kind_str(v.kind))
    .bind(&v.backend_native_id)
    .bind(v.size_bytes)
    .bind(v.used_bytes)
    .bind(&v.state)
    .bind(health_str(v.health))
    .bind(policy_id)
    .bind(&v.kubernetes_namespace)
    .bind(&v.pvc_name)
    .bind(&v.storage_class_name)
    .bind(now_rfc3339(chrono::Utc::now()))
    .execute(pool)
    .await?;
    Ok(())
}

/// Link a volume to the snapshot it was cloned/restored from (dependency tracking).
pub async fn set_volume_source_snapshot(
    pool: &AnyPool,
    volume_id: &str,
    snapshot_id: &str,
) -> Result<()> {
    sqlx::query("UPDATE storage_volumes SET source_snapshot_id=$1 WHERE id=$2")
        .bind(snapshot_id)
        .bind(volume_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// How many volumes were cloned/restored from a snapshot (blocks unsafe snapshot deletion).
pub async fn count_snapshot_dependents(pool: &AnyPool, snapshot_id: &str) -> Result<i64> {
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM storage_volumes WHERE source_snapshot_id=$1")
            .bind(snapshot_id)
            .fetch_one(pool)
            .await?;
    Ok(n)
}

/// Update a volume's provisioned size (after an expand/resize).
pub async fn set_volume_size(pool: &AnyPool, id: &str, size_bytes: i64) -> Result<()> {
    sqlx::query("UPDATE storage_volumes SET size_bytes=$1, updated_at=$2 WHERE id=$3")
        .bind(size_bytes)
        .bind(now_rfc3339(chrono::Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record a volume's applied QoS limits under `metadata.qos` (day-2 throttling). The merge
/// (read-modify-write instead of SQLite's `json_set(...)`) happens in Rust rather than server-side
/// — SQLite's JSON1 functions (`json_set`/`json_extract`/...) have no portable equivalent whose
/// query *text* is identical on Postgres (Postgres's `jsonb_set`/`->>` use a different path syntax
/// entirely), so — like the date-math sites elsewhere in this migration — this one small race
/// window (a concurrent metadata writer between the SELECT and UPDATE) is traded for portability.
pub async fn set_volume_qos(
    pool: &AnyPool,
    id: &str,
    iops_limit: Option<i64>,
    bps_limit: Option<i64>,
) -> Result<()> {
    let qos = serde_json::json!({ "iops_limit": iops_limit, "bps_limit": bps_limit });
    let current: Option<String> =
        sqlx::query_scalar("SELECT metadata FROM storage_volumes WHERE id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let mut metadata: serde_json::Value = current
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !metadata.is_object() {
        metadata = serde_json::json!({});
    }
    metadata["qos"] = qos;
    sqlx::query("UPDATE storage_volumes SET metadata=$1, updated_at=$2 WHERE id=$3")
        .bind(metadata.to_string())
        .bind(now_rfc3339(chrono::Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Update a volume's actual used (allocated) bytes.
pub async fn set_volume_used(pool: &AnyPool, id: &str, used_bytes: i64) -> Result<()> {
    sqlx::query("UPDATE storage_volumes SET used_bytes=$1, updated_at=$2 WHERE id=$3")
        .bind(used_bytes)
        .bind(now_rfc3339(chrono::Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Update just the state of a volume (e.g. to `deleting`).
pub async fn set_volume_state(pool: &AnyPool, id: &str, state: &str) -> Result<()> {
    sqlx::query("UPDATE storage_volumes SET state=$1, updated_at=$2 WHERE id=$3")
        .bind(state)
        .bind(now_rfc3339(chrono::Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Delete a volume row (after the backend resource is gone).
pub async fn delete_volume_row(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM storage_volumes WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Ids of every inventory volume attached to `pool_id`.
pub async fn volume_ids_in_pool(pool: &AnyPool, pool_id: &str) -> Result<Vec<String>> {
    let rows = sqlx::query("SELECT id FROM storage_volumes WHERE pool_id=$1")
        .bind(pool_id)
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|r| r.try_get::<String, _>("id").map_err(Into::into))
        .collect()
}

/// Whether a pool row with this id exists in inventory.
pub async fn pool_row_exists(pool: &AnyPool, pool_id: &str) -> Result<bool> {
    let row = sqlx::query("SELECT id FROM storage_pools WHERE id=$1")
        .bind(pool_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// Delete a pool row and its volume rows (after the backend pool itself is gone).
pub async fn delete_pool_with_volumes(pool: &AnyPool, pool_id: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM storage_volumes WHERE pool_id=$1")
        .bind(pool_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM storage_pools WHERE id=$1")
        .bind(pool_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Rename a volume's primary key, re-pointing every table that references it (foreign-key
/// constrained or not) in one transaction — used when a backend-side move changes what a
/// deterministic id derives to (e.g. `rbd migrate` moving an image to a new pool: its id is
/// `vol_{pool}_{name}`, so the pool change alone makes the old id stale) but the row itself is
/// still the same logical volume, just relocated.
///
/// `new_backend_native_id` is usually the *new* native id (e.g. `rbd migrate` moving an image to
/// a new pool changes it) — but it may also be the *same* value as the old row's, when the
/// caller's only goal is re-pointing which id owns an unchanged real resource (e.g.
/// `upsert_volume` reconciling a discovery-won race onto the id its own caller already promised).
/// Either way, the old row's own claim on `backend_native_id` is released (set NULL) before the
/// new row is inserted, so migrations/0027's partial UNIQUE index never sees both rows holding
/// the same non-null value at once — needed for the same-value case, and harmless for the
/// different-value case the release predates.
///
/// Inserts the new row before re-pointing children so FOREIGN KEY constraints (storage_snapshots,
/// snapshot_schedules) never see a moment where they'd reference a missing parent; the old row is
/// only deleted once nothing points at it anymore.
pub async fn rename_volume_id(
    pool: &AnyPool,
    old_id: &str,
    new_id: &str,
    new_backend_native_id: &str,
) -> Result<()> {
    if old_id == new_id {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE storage_volumes SET backend_native_id = NULL WHERE id = $1")
        .bind(old_id)
        .execute(&mut *tx)
        .await?;
    // updated_at is stamped fresh (not copied) so the renamed row isn't immediately eligible for
    // upsert_discovery's stale-volume pruning below, which compares updated_at against the
    // current discovery pass's timestamp.
    sqlx::query(
        "INSERT INTO storage_volumes
            (id, tenant_id, backend_id, cluster_id, pool_id, name, kind, backend_native_id,
             size_bytes, used_bytes, state, health, policy_id, encryption_state,
             kubernetes_namespace, pvc_name, storage_class_name, metadata, created_at, updated_at)
         SELECT $1, tenant_id, backend_id, cluster_id, pool_id, name, kind, $2,
             size_bytes, used_bytes, state, health, policy_id, encryption_state,
             kubernetes_namespace, pvc_name, storage_class_name, metadata, created_at,
             $4
         FROM storage_volumes WHERE id = $3",
    )
    .bind(new_id)
    .bind(new_backend_native_id)
    .bind(old_id)
    .bind(now_rfc3339(chrono::Utc::now()))
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE storage_snapshots SET volume_id=$1 WHERE volume_id=$2")
        .bind(new_id)
        .bind(old_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE snapshot_schedules SET volume_id=$1 WHERE volume_id=$2")
        .bind(new_id)
        .bind(old_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE product_bindings SET storage_resource_id=$1
          WHERE storage_resource_type='volume' AND storage_resource_id=$2",
    )
    .bind(new_id)
    .bind(old_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE dr_mirrors SET volume_id=$1 WHERE volume_id=$2")
        .bind(new_id)
        .bind(old_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM storage_volumes WHERE id=$1")
        .bind(old_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Record product ownership of a storage resource (PDF §5.5 ownership mapping).
#[allow(clippy::too_many_arguments)]
pub async fn insert_binding(
    pool: &AnyPool,
    id: &str,
    tenant_id: &str,
    product: &str,
    resource_type: &str,
    resource_id: &str,
    storage_resource_type: &str,
    storage_resource_id: &str,
    role: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO product_bindings
            (id, tenant_id, product, resource_type, resource_id, storage_resource_type, storage_resource_id, role)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT(product, resource_type, resource_id, storage_resource_type, storage_resource_id, role) DO NOTHING",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(product)
    .bind(resource_type)
    .bind(resource_id)
    .bind(storage_resource_type)
    .bind(storage_resource_id)
    .bind(role)
    .execute(pool)
    .await?;
    Ok(())
}

/// List the product ownership bindings for a storage resource (e.g. a volume), newest first.
pub async fn list_bindings_for(
    pool: &AnyPool,
    storage_resource_type: &str,
    storage_resource_id: &str,
) -> Result<Vec<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT id, tenant_id, product, resource_type, resource_id, role, created_at
         FROM product_bindings
         WHERE storage_resource_type = $1 AND storage_resource_id = $2
         ORDER BY created_at DESC",
    )
    .bind(storage_resource_type)
    .bind(storage_resource_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.get::<String, _>("id"),
                "tenant_id": r.get::<String, _>("tenant_id"),
                "product": r.get::<String, _>("product"),
                "resource_type": r.get::<String, _>("resource_type"),
                "resource_id": r.get::<String, _>("resource_id"),
                "role": r.get::<String, _>("role"),
                "created_at": r.get::<String, _>("created_at"),
            })
        })
        .collect())
}

/// Persist a full discovery pass for `backend_id`, upserting cluster, pools, osds and volumes.
/// `authoritative`: false for a fixture-backed driver (see `StorageDriver::is_fixture`) — its
/// volume list is a fixed snapshot, not a complete rescan, so stale-volume pruning below is
/// skipped rather than deleting anything the fixture simply didn't happen to mention.
pub async fn upsert_discovery(
    pool: &AnyPool,
    backend_id: &str,
    d: &DiscoveryResult,
    authoritative: bool,
) -> Result<()> {
    let mut tx = pool.begin().await?;

    // cluster
    sqlx::query(
        "INSERT INTO storage_clusters
            (id, backend_id, native_fsid, name, health, raw_capacity_bytes, used_capacity_bytes, available_capacity_bytes, discovered_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT(id) DO UPDATE SET
            native_fsid=excluded.native_fsid, name=excluded.name, health=excluded.health,
            raw_capacity_bytes=excluded.raw_capacity_bytes, used_capacity_bytes=excluded.used_capacity_bytes,
            available_capacity_bytes=excluded.available_capacity_bytes, discovered_at=excluded.discovered_at",
    )
    .bind(&d.cluster.id)
    .bind(backend_id)
    .bind(&d.cluster.native_fsid)
    .bind(&d.cluster.name)
    .bind(health_str(d.cluster.health))
    .bind(d.cluster.raw_capacity_bytes)
    .bind(d.cluster.used_capacity_bytes)
    .bind(d.cluster.available_capacity_bytes)
    .bind(now_rfc3339(chrono::Utc::now()))
    .execute(&mut *tx)
    .await?;

    for p in &d.pools {
        sqlx::query(
            "INSERT INTO storage_pools (id, cluster_id, name, kind, device_class, replica_size, used_bytes, max_bytes, health)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT(cluster_id, name) DO UPDATE SET
                kind=excluded.kind, device_class=excluded.device_class, replica_size=excluded.replica_size,
                used_bytes=excluded.used_bytes, max_bytes=excluded.max_bytes, health=excluded.health",
        )
        .bind(&p.id)
        .bind(&p.cluster_id)
        .bind(&p.name)
        .bind(&p.kind)
        .bind(&p.device_class)
        .bind(p.replica_size)
        .bind(p.used_bytes)
        .bind(p.max_bytes)
        .bind(health_str(p.health))
        .execute(&mut *tx)
        .await?;
    }

    for o in &d.osds {
        sqlx::query(
            "INSERT INTO storage_osds (id, cluster_id, osd_num, up, in_cluster, device_class, host, used_bytes, capacity_bytes)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT(cluster_id, osd_num) DO UPDATE SET
                up=excluded.up, in_cluster=excluded.in_cluster, device_class=excluded.device_class,
                host=excluded.host, used_bytes=excluded.used_bytes, capacity_bytes=excluded.capacity_bytes",
        )
        .bind(format!("osd_{}_{}", o.cluster_id, o.id))
        .bind(&o.cluster_id)
        .bind(o.id)
        .bind(o.up as i64)
        .bind(o.in_cluster as i64)
        .bind(&o.device_class)
        .bind(&o.host)
        .bind(o.used_bytes)
        .bind(o.capacity_bytes)
        .execute(&mut *tx)
        .await?;
    }

    // Marker taken before upserting this discovery's volumes: every volume the pass touches gets
    // a newer `updated_at`, so anything left older is stale (deleted from the backend) and can be
    // pruned below.
    let discovery_ts: String = now_rfc3339(chrono::Utc::now());

    for v in &d.volumes {
        // Resolve to an already-inventoried row's id when this discovered volume is the same real
        // resource under a different id. Driver-derived ids don't always match the id minted at
        // creation time — e.g. `POST /volumes` mints a random id, but discovery derives
        // `vol_{pool}_{image}` for that same RBD image. Without this, an authoritative pass would
        // insert a *duplicate* row under the new id while the original — the id every label,
        // quota-count, DR mirror, and snapshot schedule still points at — goes untouched by this
        // pass and gets pruned below as stale, permanently orphaning it. Verified live: a
        // PVC-created volume vanished from inventory entirely while its PVC stayed Bound on the
        // real cluster. `backend_native_id` is the general anchor (unique when set, migration
        // 0027); `(kubernetes_namespace, pvc_name)` is the fallback for the rare case a volume's
        // native id isn't populated yet.
        let canonical_id: String = if let Some(native) = v.backend_native_id.as_deref() {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM storage_volumes WHERE backend_native_id = $1 AND id != $2 LIMIT 1",
            )
            .bind(native)
            .bind(&v.id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or_else(|| v.id.clone())
        } else if let (Some(ns), Some(pvc)) = (&v.kubernetes_namespace, &v.pvc_name) {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM storage_volumes
                  WHERE kubernetes_namespace = $1 AND pvc_name = $2 AND id != $3 LIMIT 1",
            )
            .bind(ns)
            .bind(pvc)
            .bind(&v.id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or_else(|| v.id.clone())
        } else {
            v.id.clone()
        };
        sqlx::query(
            "INSERT INTO storage_volumes
                (id, backend_id, cluster_id, pool_id, name, kind, backend_native_id, size_bytes, used_bytes,
                 state, health, kubernetes_namespace, pvc_name, storage_class_name, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
             ON CONFLICT(id) DO UPDATE SET
                cluster_id=excluded.cluster_id, pool_id=excluded.pool_id, name=excluded.name, kind=excluded.kind,
                backend_native_id=excluded.backend_native_id, size_bytes=excluded.size_bytes, used_bytes=excluded.used_bytes,
                state=excluded.state, health=excluded.health,
                -- Preserve Kubernetes correlation across re-discovery: a driver pass that can't
                -- attribute the RBD image (e.g. the periodic monitor, which has no K8s client)
                -- reports NULL — keep the previously enriched value instead of wiping it.
                kubernetes_namespace=COALESCE(excluded.kubernetes_namespace, storage_volumes.kubernetes_namespace),
                pvc_name=COALESCE(excluded.pvc_name, storage_volumes.pvc_name),
                storage_class_name=COALESCE(excluded.storage_class_name, storage_volumes.storage_class_name),
                updated_at=excluded.updated_at",
        )
        .bind(&canonical_id)
        .bind(backend_id)
        .bind(&v.cluster_id)
        .bind(&v.pool_id)
        .bind(&v.name)
        .bind(volume_kind_str(v.kind))
        .bind(&v.backend_native_id)
        .bind(v.size_bytes)
        .bind(v.used_bytes)
        .bind(&v.state)
        .bind(health_str(v.health))
        .bind(&v.kubernetes_namespace)
        .bind(&v.pvc_name)
        .bind(&v.storage_class_name)
        .bind(now_rfc3339(chrono::Utc::now()))
        .execute(&mut *tx)
        .await?;
    }

    // Prune block volumes this backend no longer reports (deleted from Ceph) so orphaned inventory
    // doesn't linger. Protective: never touch volumes a product owns (product_bindings) or that a
    // snapshot depends on. Self-healing — a transient miss just re-adds the row next discovery.
    // Skipped entirely for a fixture-backed driver: its volume list never grows to include
    // anything created directly via a job (e.g. a raw RBD image), so this would otherwise delete
    // every such volume on the very next discovery pass.
    if authoritative {
        let pruned = sqlx::query(
            "DELETE FROM storage_volumes
              WHERE backend_id = $1 AND kind = 'block' AND updated_at < $2
                AND id NOT IN (
                    SELECT storage_resource_id FROM product_bindings WHERE storage_resource_type = 'volume'
                )
                AND id NOT IN (SELECT volume_id FROM storage_snapshots)",
        )
        .bind(backend_id)
        .bind(&discovery_ts)
        .execute(&mut *tx)
        .await?;
        if pruned.rows_affected() > 0 {
            tracing::info!(
                backend = backend_id,
                pruned = pruned.rows_affected(),
                "discovery.pruned_stale_volumes"
            );
        }
    }

    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Policy drift (day-2 governance): volumes whose applied StorageClass no longer matches the
/// StorageClass their assigned policy resolves to, or whose policy was deleted.
pub async fn list_policy_drift(pool: &AnyPool) -> Result<Vec<serde_json::Value>> {
    let rows = sqlx::query(
        // `placement` (raw JSON text) is pulled back and parsed in Rust rather than extracted via
        // SQLite's json_extract() server-side — no portable equivalent exists whose query text is
        // identical on Postgres (see set_volume_qos's comment for the same tradeoff).
        "SELECT v.id AS volume_id, v.name AS name, v.storage_class_name AS actual, v.policy_id AS policy_id,
                p.placement AS placement,
                CASE WHEN p.id IS NULL THEN 1 ELSE 0 END AS policy_missing
         FROM storage_volumes v LEFT JOIN storage_policies p ON p.id = v.policy_id
         WHERE v.policy_id IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    let mut drift = Vec::new();
    for r in rows {
        let actual: Option<String> = r.get("actual");
        let placement: Option<String> = r.get("placement");
        let expected: Option<String> = placement.as_deref().and_then(|p| {
            serde_json::from_str::<serde_json::Value>(p)
                .ok()
                .and_then(|v| {
                    v.get("storage_class")
                        .and_then(|s| s.as_str())
                        .map(str::to_string)
                })
        });
        let missing: i64 = r.get("policy_missing");
        let drifted = missing == 1 || (expected.is_some() && actual != expected);
        if drifted {
            drift.push(serde_json::json!({
                "volume_id": r.get::<String, _>("volume_id"),
                "name": r.get::<String, _>("name"),
                "policy_id": r.get::<Option<String>, _>("policy_id"),
                "expected_storage_class": expected,
                "actual_storage_class": actual,
                "reason": if missing == 1 { "policy deleted" } else { "storage class differs from policy" },
            }));
        }
    }
    Ok(drift)
}

/// Cordon or uncordon a backend (day-2 maintenance). Returns whether a row changed.
pub async fn set_backend_cordoned(pool: &AnyPool, id: &str, cordoned: bool) -> Result<bool> {
    let res = sqlx::query("UPDATE storage_backends SET cordoned=$1, updated_at=$2 WHERE id=$3")
        .bind(cordoned as i64)
        .bind(now_rfc3339(chrono::Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Whether a backend is cordoned (rejects new provisioning). Missing backend → not cordoned.
pub async fn is_backend_cordoned(pool: &AnyPool, id: &str) -> Result<bool> {
    let v: Option<i64> = sqlx::query_scalar("SELECT cordoned FROM storage_backends WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(v.unwrap_or(0) != 0)
}

/// Number of volumes still referencing a backend — a guard against deleting an in-use backend.
pub async fn backend_volume_count(pool: &AnyPool, id: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM storage_volumes WHERE backend_id=$1")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

/// Delete a backend inventory row (e.g. a decommissioned or fixture backend). Callers should refuse
/// when [`backend_volume_count`] is non-zero so live volumes aren't orphaned.
pub async fn delete_backend(pool: &AnyPool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM storage_backends WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Purge a backend's leftover *inventory* volume rows without touching any real storage — for a
/// decommissioned/fixture backend whose discovered volumes (e.g. NFS exports) have no live driver.
pub async fn delete_volumes_by_backend(pool: &AnyPool, backend_id: &str) -> Result<u64> {
    let r = sqlx::query("DELETE FROM storage_volumes WHERE backend_id=$1")
        .bind(backend_id)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

fn row_to_backend(r: sqlx::any::AnyRow) -> StorageBackend {
    let caps: Capabilities =
        serde_json::from_str(r.get::<String, _>("capabilities").as_str()).unwrap_or_default();
    StorageBackend {
        id: r.get("id"),
        name: r.get("name"),
        backend_type: backend_type_from(r.get::<String, _>("backend_type").as_str()),
        mode: mode_from(r.get::<String, _>("mode").as_str()),
        status: r.get("status"),
        capabilities: caps,
        connection_ref: r.get("connection_ref"),
        cordoned: r.get::<i64, _>("cordoned") != 0,
    }
}

pub async fn list_backends(pool: &AnyPool) -> Result<Vec<StorageBackend>> {
    let rows = sqlx::query(
        "SELECT id, name, backend_type, mode, status, capabilities, connection_ref, cordoned FROM storage_backends ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_backend).collect())
}

/// A single backend row by id (`None` if not registered) — used to resolve a backend's
/// `connection_ref` (credentials secret name) at request/dispatch time, e.g. for a bring-your-own S3
/// object-storage write path.
pub async fn get_backend(pool: &AnyPool, id: &str) -> Result<Option<StorageBackend>> {
    let row = sqlx::query(
        "SELECT id, name, backend_type, mode, status, capabilities, connection_ref, cordoned FROM storage_backends WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_backend))
}

pub async fn list_clusters(pool: &AnyPool) -> Result<Vec<StorageCluster>> {
    let rows = sqlx::query(
        "SELECT id, backend_id, native_fsid, name, health, raw_capacity_bytes, used_capacity_bytes, available_capacity_bytes
         FROM storage_clusters ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_cluster).collect())
}

pub async fn get_cluster(pool: &AnyPool, id: &str) -> Result<Option<StorageCluster>> {
    let row = sqlx::query(
        "SELECT id, backend_id, native_fsid, name, health, raw_capacity_bytes, used_capacity_bytes, available_capacity_bytes
         FROM storage_clusters WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_cluster))
}

/// Derive a health snapshot for a cluster from its stored row.
pub async fn cluster_health(pool: &AnyPool, id: &str) -> Result<Option<StorageHealth>> {
    Ok(get_cluster(pool, id).await?.map(|c| StorageHealth {
        status: c.health,
        summary: health_str(c.health).to_uppercase(),
        raw_capacity_bytes: c.raw_capacity_bytes,
        used_capacity_bytes: c.used_capacity_bytes,
        available_capacity_bytes: c.available_capacity_bytes,
        recovering: false,
        degraded_objects: 0,
    }))
}

fn row_to_cluster(r: sqlx::any::AnyRow) -> StorageCluster {
    StorageCluster {
        id: r.get("id"),
        backend_id: r.get("backend_id"),
        native_fsid: r.get("native_fsid"),
        name: r.get("name"),
        health: health_from(r.get::<String, _>("health").as_str()),
        raw_capacity_bytes: r.get("raw_capacity_bytes"),
        used_capacity_bytes: r.get("used_capacity_bytes"),
        available_capacity_bytes: r.get("available_capacity_bytes"),
    }
}

pub async fn list_pools(pool: &AnyPool) -> Result<Vec<StoragePool>> {
    list_pools_filtered(pool, None, None).await
}

/// List pools, optionally filtered by owning backend (via the cluster join) and/or pool `kind`.
pub async fn list_pools_filtered(
    pool: &AnyPool,
    backend_id: Option<&str>,
    kind: Option<&str>,
) -> Result<Vec<StoragePool>> {
    let rows = sqlx::query(
        "SELECT id, cluster_id, name, kind, device_class, replica_size, used_bytes, max_bytes, health
         FROM storage_pools
         WHERE ($1 IS NULL OR kind = $2)
           AND ($3 IS NULL OR cluster_id IN (SELECT id FROM storage_clusters WHERE backend_id = $4))
         ORDER BY name",
    )
    .bind(kind)
    .bind(kind)
    .bind(backend_id)
    .bind(backend_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| StoragePool {
            id: r.get("id"),
            cluster_id: r.get("cluster_id"),
            name: r.get("name"),
            kind: r.get("kind"),
            device_class: r.get("device_class"),
            replica_size: r.get("replica_size"),
            used_bytes: r.get("used_bytes"),
            max_bytes: r.get("max_bytes"),
            health: health_from(
                r.get::<Option<String>, _>("health")
                    .unwrap_or_default()
                    .as_str(),
            ),
        })
        .collect())
}

pub async fn list_osds(pool: &AnyPool) -> Result<Vec<Osd>> {
    let rows = sqlx::query(
        "SELECT cluster_id, osd_num, up, in_cluster, device_class, host, used_bytes, capacity_bytes
         FROM storage_osds ORDER BY cluster_id, osd_num",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Osd {
            id: r.get("osd_num"),
            cluster_id: r.get("cluster_id"),
            up: r.get::<i64, _>("up") != 0,
            in_cluster: r.get::<i64, _>("in_cluster") != 0,
            device_class: r.get("device_class"),
            host: r.get("host"),
            used_bytes: r.get("used_bytes"),
            capacity_bytes: r.get("capacity_bytes"),
        })
        .collect())
}

pub async fn list_volumes(pool: &AnyPool) -> Result<Vec<StorageVolume>> {
    let rows = sqlx::query(volume_select("ORDER BY name"))
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row_to_volume).collect())
}

/// Count volumes still provisioned against a StorageClass — the dependent-guard behind Rook pool/
/// filesystem/object-store delete (`DELETE /ceph/{pools,filesystems,object-stores}/{name}`),
/// mirroring `backups::count_for_bucket`'s guard on bucket delete.
pub async fn count_volumes_by_storage_class(pool: &AnyPool, storage_class: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM storage_volumes WHERE storage_class_name = $1")
            .bind(storage_class)
            .fetch_one(pool)
            .await?,
    )
}

/// Merge `labels` (a JSON object) into a volume's `metadata.labels`. Returns the merged label map.
pub async fn set_volume_labels(
    pool: &AnyPool,
    id: &str,
    labels: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value> {
    let existing: Option<String> =
        sqlx::query_scalar("SELECT metadata FROM storage_volumes WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let mut meta: serde_json::Value = existing
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let map = meta
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("volume metadata is not an object"))?;
    let current = map.entry("labels").or_insert_with(|| serde_json::json!({}));
    if let Some(cur) = current.as_object_mut() {
        for (k, v) in labels {
            cur.insert(k.clone(), v.clone());
        }
    }
    let merged = map.get("labels").cloned().unwrap_or(serde_json::json!({}));
    sqlx::query("UPDATE storage_volumes SET metadata = $1, updated_at = $2 WHERE id = $3")
        .bind(meta.to_string())
        .bind(now_rfc3339(chrono::Utc::now()))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(merged)
}

/// Read a volume's `metadata.labels` (empty object if none).
pub async fn get_volume_labels(pool: &AnyPool, id: &str) -> Result<serde_json::Value> {
    let meta: Option<String> =
        sqlx::query_scalar("SELECT metadata FROM storage_volumes WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let v: serde_json::Value = meta
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    Ok(v.get("labels").cloned().unwrap_or(serde_json::json!({})))
}

/// The backend a volume belongs to (the DTO omits it).
pub async fn volume_backend_id(pool: &AnyPool, id: &str) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT backend_id FROM storage_volumes WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

/// The owning tenant of a volume (the DTO omits it); defaults to `global` if the volume is gone.
pub async fn volume_tenant(pool: &AnyPool, id: &str) -> Result<String> {
    let t: Option<String> =
        sqlx::query_scalar("SELECT tenant_id FROM storage_volumes WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(t.unwrap_or_else(|| "global".into()))
}

/// List volumes with optional `state` / `tenant_id` equality filters.
pub async fn list_volumes_filtered(
    pool: &AnyPool,
    state: Option<&str>,
    tenant_id: Option<&str>,
    backend_id: Option<&str>,
    kind: Option<&str>,
) -> Result<Vec<StorageVolume>> {
    let rows = sqlx::query(volume_select(
        "WHERE ($1 IS NULL OR state = $2) AND ($3 IS NULL OR tenant_id = $4)
           AND ($5 IS NULL OR backend_id = $6) AND ($7 IS NULL OR kind = $8) ORDER BY name",
    ))
    .bind(state)
    .bind(state)
    .bind(tenant_id)
    .bind(tenant_id)
    .bind(backend_id)
    .bind(backend_id)
    .bind(kind)
    .bind(kind)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_volume).collect())
}

pub async fn get_volume(pool: &AnyPool, id: &str) -> Result<Option<StorageVolume>> {
    let row = sqlx::query(volume_select("WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(row_to_volume))
}

// `tail` is only ever a string literal from this module's own call sites (never user input — user
// data always goes through `$N` bind parameters), so asserting the assembled SQL safe is sound.
fn volume_select(tail: &str) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(format!(
        "SELECT id, cluster_id, pool_id, name, kind, backend_native_id, size_bytes, used_bytes, state, health,
                kubernetes_namespace, pvc_name, storage_class_name
         FROM storage_volumes {tail}"
    ))
}

/// List volumes owned by a product (via `product_bindings`), optionally scoped to a single owning
/// resource id. Used by the gRPC edge so a product can enumerate only the volumes it owns.
pub async fn list_volumes_by_owner(
    pool: &AnyPool,
    product: &str,
    resource_id: Option<&str>,
) -> Result<Vec<StorageVolume>> {
    let sql = "SELECT DISTINCT v.id, v.cluster_id, v.pool_id, v.name, v.kind, v.backend_native_id,
                      v.size_bytes, v.used_bytes, v.state, v.health, v.kubernetes_namespace,
                      v.pvc_name, v.storage_class_name
               FROM storage_volumes v
               JOIN product_bindings b
                 ON b.storage_resource_type = 'volume' AND b.storage_resource_id = v.id
               WHERE b.product = $1 AND ($2 IS NULL OR b.resource_id = $3)
               ORDER BY v.name";
    let rows = sqlx::query(sql)
        .bind(product)
        .bind(resource_id)
        .bind(resource_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(row_to_volume).collect())
}

fn row_to_volume(r: sqlx::any::AnyRow) -> StorageVolume {
    StorageVolume {
        id: r.get("id"),
        cluster_id: r.get("cluster_id"),
        pool_id: r.get("pool_id"),
        name: r.get("name"),
        kind: volume_kind_from(r.get::<String, _>("kind").as_str()),
        backend_native_id: r.get("backend_native_id"),
        size_bytes: r.get("size_bytes"),
        used_bytes: r.get("used_bytes"),
        state: r.get("state"),
        health: health_from(r.get::<String, _>("health").as_str()),
        kubernetes_namespace: r.get("kubernetes_namespace"),
        pvc_name: r.get("pvc_name"),
        storage_class_name: r.get("storage_class_name"),
    }
}

/// Per-backend inventory breakdown (backend type, cluster/volume counts, capacity) so the
/// multi-backend picture is distinguishable in the API and Prometheus.
pub async fn backend_breakdown(pool: &AnyPool) -> Result<Vec<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT b.id AS id, b.backend_type AS backend_type, b.mode AS mode, b.status AS status,
                (SELECT COUNT(*) FROM storage_clusters c WHERE c.backend_id=b.id) AS clusters,
                (SELECT COUNT(*) FROM storage_volumes v WHERE v.backend_id=b.id) AS volumes,
                CAST(COALESCE((SELECT SUM(raw_capacity_bytes)  FROM storage_clusters c WHERE c.backend_id=b.id),0) AS BIGINT) AS raw,
                CAST(COALESCE((SELECT SUM(used_capacity_bytes) FROM storage_clusters c WHERE c.backend_id=b.id),0) AS BIGINT) AS used
         FROM storage_backends b ORDER BY b.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "backend_id": r.get::<String, _>("id"),
                "backend_type": r.get::<String, _>("backend_type"),
                "mode": r.get::<String, _>("mode"),
                "status": r.get::<String, _>("status"),
                "clusters": r.get::<i64, _>("clusters"),
                "volumes": r.get::<i64, _>("volumes"),
                "raw_capacity_bytes": r.get::<i64, _>("raw"),
                "used_capacity_bytes": r.get::<i64, _>("used"),
            })
        })
        .collect())
}

/// Aggregate capacity summary across all clusters (PDF §13.2 overview cards).
pub async fn metrics_summary(pool: &AnyPool) -> Result<serde_json::Value> {
    let row = sqlx::query(
        // CAST(... AS BIGINT): Postgres's SUM(bigint) returns NUMERIC (overflow-safe by
        // default), which sqlx's Any driver can't decode — force a plain bigint result,
        // portable to SQLite (CAST AS BIGINT there just gets INTEGER affinity).
        "SELECT
            CAST(COALESCE(SUM(raw_capacity_bytes),0) AS BIGINT)       AS raw,
            CAST(COALESCE(SUM(used_capacity_bytes),0) AS BIGINT)      AS used,
            CAST(COALESCE(SUM(available_capacity_bytes),0) AS BIGINT) AS avail,
            COUNT(*)                                  AS clusters
         FROM storage_clusters",
    )
    .fetch_one(pool)
    .await?;
    let volumes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage_volumes")
        .fetch_one(pool)
        .await?;
    let pools_n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage_pools")
        .fetch_one(pool)
        .await?;
    let snapshots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage_snapshots")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    let buckets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage_buckets")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    let backups: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage_backups")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    // Client I/O + recovery rollups from the latest scraped Ceph metrics (0 if none yet).
    let raw = row.get::<i64, _>("raw");
    let used = row.get::<i64, _>("used");
    let used_pct = if raw > 0 {
        (used as f64 / raw as f64 * 10_000.0).round() / 100.0
    } else {
        0.0
    };
    async fn sum(pool: &AnyPool, n: &str) -> f64 {
        metrics::sum_value(pool, n).await.unwrap_or(0.0)
    }
    Ok(serde_json::json!({
        "raw_capacity_bytes": raw,
        "used_capacity_bytes": used,
        "available_capacity_bytes": row.get::<i64, _>("avail"),
        "used_capacity_percent": used_pct,
        "clusters": row.get::<i64, _>("clusters"),
        "pools": pools_n,
        "volumes": volumes,
        "snapshots": snapshots,
        "buckets": buckets,
        "backups": backups,
        "client_io": {
            "read_ops_total": sum(pool, "ceph_pool_rd").await,
            "write_ops_total": sum(pool, "ceph_pool_wr").await,
            "read_bytes_total": sum(pool, "ceph_pool_rd_bytes").await,
            "write_bytes_total": sum(pool, "ceph_pool_wr_bytes").await,
        },
        "recovery": {
            "pg_recovering": sum(pool, "ceph_pg_recovering").await,
            "pg_backfilling": sum(pool, "ceph_pg_backfilling").await,
            "objects_degraded": sum(pool, "ceph_num_objects_degraded").await,
            "objects_misplaced": sum(pool, "ceph_num_objects_misplaced").await,
            "objects_unfound": sum(pool, "ceph_num_objects_unfound").await,
        },
    }))
}

#[cfg(test)]
mod connect_tests {
    // `connect()` used to hard-reject `postgres://` URLs (pre-`sqlx::Any` migration, see
    // docs/HA.md). It now dispatches to the real Postgres driver instead — proven live against a
    // real Postgres in crates/atlas-inventory/tests/postgres_live.rs (`--ignored`, needs
    // infra). What's safe to assert without live infra is scheme detection itself.
    #[test]
    fn is_postgres_url_detects_scheme_without_connecting() {
        assert!(super::is_postgres_url(
            "postgres://atlas:atlas@127.0.0.1:5432/atlas"
        ));
        assert!(super::is_postgres_url(
            "postgresql://atlas:atlas@127.0.0.1:5432/atlas"
        ));
        assert!(!super::is_postgres_url("sqlite:///tmp/atlas.db?mode=rwc"));
    }
}
