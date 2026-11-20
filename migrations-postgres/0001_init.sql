-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Atlas storage control plane — initial schema (PostgreSQL dialect).
-- Phase-1 HA: Postgres dialect of migrations/0001_init.sql.
-- Timestamps stay TEXT (RFC3339 UTC via to_char) to minimize query rewrite.
-- JSON columns stay TEXT with ::json CHECK (not JSONB yet).

CREATE TABLE IF NOT EXISTS storage_backends (
    id            TEXT PRIMARY KEY,
    tenant_scope  TEXT NOT NULL DEFAULT 'global',
    backend_type  TEXT NOT NULL,                         -- ceph, nfs, zfs, san, cloud_block, kubernetes
    mode          TEXT NOT NULL,                         -- managed_rook, external, read_only
    name          TEXT NOT NULL,
    status        TEXT NOT NULL,
    capabilities  TEXT NOT NULL DEFAULT '{}' CHECK ((capabilities)::json IS NOT NULL),
    connection_ref TEXT,                                 -- secret reference, never a raw secret
    created_at    TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    updated_at    TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
);

CREATE TABLE IF NOT EXISTS storage_clusters (
    id                       TEXT PRIMARY KEY,
    backend_id               TEXT NOT NULL REFERENCES storage_backends(id) ON DELETE CASCADE,
    native_fsid              TEXT,
    name                     TEXT NOT NULL,
    health                   TEXT NOT NULL DEFAULT 'unknown',
    raw_capacity_bytes       INTEGER,
    used_capacity_bytes      INTEGER,
    available_capacity_bytes INTEGER,
    details                  TEXT NOT NULL DEFAULT '{}' CHECK ((details)::json IS NOT NULL),
    discovered_at            TEXT
);

CREATE TABLE IF NOT EXISTS storage_pools (
    id                   TEXT PRIMARY KEY,
    cluster_id           TEXT NOT NULL REFERENCES storage_clusters(id) ON DELETE CASCADE,
    name                 TEXT NOT NULL,
    kind                 TEXT NOT NULL,                  -- rbd, cephfs_data, cephfs_metadata, rgw, other
    device_class         TEXT,
    replica_size         INTEGER,
    erasure_code_profile TEXT,
    used_bytes           INTEGER,
    max_bytes            INTEGER,
    health               TEXT,
    labels               TEXT NOT NULL DEFAULT '{}' CHECK ((labels)::json IS NOT NULL),
    UNIQUE (cluster_id, name)
);

CREATE TABLE IF NOT EXISTS storage_osds (
    id             TEXT PRIMARY KEY,                     -- osd stable id (e.g. osd_<cluster>_<n>)
    cluster_id     TEXT NOT NULL REFERENCES storage_clusters(id) ON DELETE CASCADE,
    osd_num        INTEGER NOT NULL,
    up             INTEGER NOT NULL DEFAULT 0,
    in_cluster     INTEGER NOT NULL DEFAULT 0,
    device_class   TEXT,
    host           TEXT,
    used_bytes     INTEGER,
    capacity_bytes INTEGER,
    UNIQUE (cluster_id, osd_num)
);

CREATE TABLE IF NOT EXISTS storage_volumes (
    id                   TEXT PRIMARY KEY,
    tenant_id            TEXT NOT NULL DEFAULT 'global',
    backend_id           TEXT NOT NULL REFERENCES storage_backends(id) ON DELETE CASCADE,
    cluster_id           TEXT REFERENCES storage_clusters(id) ON DELETE SET NULL,
    pool_id              TEXT REFERENCES storage_pools(id) ON DELETE SET NULL,
    name                 TEXT NOT NULL,
    kind                 TEXT NOT NULL,                  -- block, filesystem, object
    backend_native_id    TEXT,
    size_bytes           INTEGER NOT NULL,
    used_bytes           INTEGER,
    state                TEXT NOT NULL,
    health               TEXT NOT NULL DEFAULT 'unknown',
    policy_id            TEXT,
    encryption_state     TEXT,
    kubernetes_namespace TEXT,
    pvc_name             TEXT,
    storage_class_name   TEXT,
    metadata             TEXT NOT NULL DEFAULT '{}' CHECK ((metadata)::json IS NOT NULL),
    created_at           TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    updated_at           TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
);

CREATE TABLE IF NOT EXISTS product_bindings (
    id                    TEXT PRIMARY KEY,
    tenant_id             TEXT NOT NULL DEFAULT 'global',
    product               TEXT NOT NULL,                 -- veyron, hyper2kvm, guestkit, ...
    resource_type         TEXT NOT NULL,
    resource_id           TEXT NOT NULL,
    storage_resource_type TEXT NOT NULL,                 -- volume, bucket, share
    storage_resource_id   TEXT NOT NULL,
    role                  TEXT NOT NULL,                 -- root_disk, data_disk, backup_bucket, iso_library
    created_at            TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    UNIQUE (product, resource_type, resource_id, storage_resource_type, storage_resource_id, role)
);

CREATE TABLE IF NOT EXISTS storage_snapshots (
    id                 TEXT PRIMARY KEY,
    tenant_id          TEXT NOT NULL DEFAULT 'global',
    volume_id          TEXT NOT NULL REFERENCES storage_volumes(id) ON DELETE CASCADE,
    name               TEXT NOT NULL,
    backend_native_id  TEXT,
    consistency        TEXT NOT NULL DEFAULT 'crash',    -- crash, app, file
    state              TEXT NOT NULL,
    protected          INTEGER NOT NULL DEFAULT 0,
    parent_snapshot_id TEXT,
    retention_until    TEXT,
    metadata           TEXT NOT NULL DEFAULT '{}' CHECK ((metadata)::json IS NOT NULL),
    created_at         TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
);

CREATE TABLE IF NOT EXISTS storage_jobs (
    id               TEXT PRIMARY KEY,
    tenant_id        TEXT NOT NULL DEFAULT 'global',
    job_type         TEXT NOT NULL,
    state            TEXT NOT NULL,                       -- pending,queued,running,verifying,succeeded,failed,...
    requested_by     TEXT NOT NULL,
    request          TEXT NOT NULL DEFAULT '{}' CHECK ((request)::json IS NOT NULL),
    result           TEXT NOT NULL DEFAULT '{}' CHECK ((result)::json IS NOT NULL),
    error            TEXT,
    progress_percent INTEGER NOT NULL DEFAULT 0,
    idempotency_key  TEXT,
    created_at       TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    updated_at       TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    started_at       TEXT,
    completed_at     TEXT
);

CREATE TABLE IF NOT EXISTS storage_policies (
    id               TEXT PRIMARY KEY,
    tenant_id        TEXT,                                -- null = global template
    name             TEXT NOT NULL,
    intent           TEXT NOT NULL,                       -- production, database, dev, archive, ai
    backend_selector TEXT NOT NULL DEFAULT '{}' CHECK ((backend_selector)::json IS NOT NULL),
    placement        TEXT NOT NULL DEFAULT '{}' CHECK ((placement)::json IS NOT NULL),
    protection       TEXT NOT NULL DEFAULT '{}' CHECK ((protection)::json IS NOT NULL),
    backup           TEXT NOT NULL DEFAULT '{}' CHECK ((backup)::json IS NOT NULL),
    performance      TEXT NOT NULL DEFAULT '{}' CHECK ((performance)::json IS NOT NULL),
    security         TEXT NOT NULL DEFAULT '{}' CHECK ((security)::json IS NOT NULL),
    created_at       TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    updated_at       TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
);

CREATE TABLE IF NOT EXISTS storage_alerts (
    id            TEXT PRIMARY KEY,
    tenant_id     TEXT,
    severity      TEXT NOT NULL,
    source        TEXT NOT NULL,
    resource_type TEXT NOT NULL,
    resource_id   TEXT NOT NULL,
    title         TEXT NOT NULL,
    description   TEXT NOT NULL,
    evidence      TEXT NOT NULL DEFAULT '{}' CHECK ((evidence)::json IS NOT NULL),
    state         TEXT NOT NULL DEFAULT 'open',
    created_at    TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    resolved_at   TEXT
);

CREATE TABLE IF NOT EXISTS storage_audit_logs (
    id            BIGSERIAL PRIMARY KEY,
    tenant_id     TEXT,
    actor_id      TEXT NOT NULL,
    action        TEXT NOT NULL,
    resource_type TEXT NOT NULL,
    resource_id   TEXT NOT NULL,
    status        TEXT NOT NULL,
    request       TEXT CHECK (request IS NULL OR (request)::json IS NOT NULL),
    result        TEXT CHECK (result IS NULL OR (result)::json IS NOT NULL),
    created_at    TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
);

-- Read-path indexes.
CREATE INDEX IF NOT EXISTS idx_pools_cluster    ON storage_pools(cluster_id);
CREATE INDEX IF NOT EXISTS idx_osds_cluster     ON storage_osds(cluster_id);
CREATE INDEX IF NOT EXISTS idx_volumes_backend  ON storage_volumes(backend_id);
CREATE INDEX IF NOT EXISTS idx_volumes_pool     ON storage_volumes(pool_id);
CREATE INDEX IF NOT EXISTS idx_bindings_storage ON product_bindings(storage_resource_type, storage_resource_id);
CREATE INDEX IF NOT EXISTS idx_audit_resource   ON storage_audit_logs(resource_type, resource_id);
