-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Zyvor DataBridge: cloud-to-edge database migration control plane.
-- Sources (cloud managed DBs) -> migration plans -> edge DB clusters on Ceph -> CDC -> cutover.

-- A registered cloud/source database (AWS RDS/Aurora, GCP Cloud SQL, or generic PG/MySQL).
CREATE TABLE IF NOT EXISTS migration_sources (
    id            TEXT PRIMARY KEY,
    tenant_id     TEXT NOT NULL DEFAULT 'global',
    name          TEXT NOT NULL,
    kind          TEXT NOT NULL,                          -- postgres | mysql
    cloud         TEXT NOT NULL DEFAULT 'generic',        -- rds | aurora | cloudsql | generic
    endpoint      TEXT,
    port          INTEGER,
    database      TEXT,
    secret_ref    TEXT,                                   -- k8s Secret with creds; never the creds
    secret_namespace TEXT,
    tls_mode      TEXT NOT NULL DEFAULT 'require',        -- disable | require | verify-full
    driver_mode   TEXT NOT NULL DEFAULT 'fake',           -- fake | real
    state         TEXT NOT NULL DEFAULT 'registered',     -- registered|discovering|discovered|error
    discovered    TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(discovered)),
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

-- The edge target database cluster (CloudNativePG / MySQL operator) on Ceph-backed storage.
CREATE TABLE IF NOT EXISTS edge_db_clusters (
    id               TEXT PRIMARY KEY,
    tenant_id        TEXT NOT NULL DEFAULT 'global',
    plan_id          TEXT,
    engine           TEXT NOT NULL,                       -- postgres | mysql
    operator         TEXT NOT NULL DEFAULT 'cnpg',        -- cnpg | percona | oracle
    namespace        TEXT NOT NULL DEFAULT 'zyvor-databridge',
    cr_name          TEXT,
    storage_class    TEXT NOT NULL DEFAULT 'zyvor-rbd-prod',
    wal_storage_class TEXT,
    instances        INTEGER NOT NULL DEFAULT 1,
    size_bytes       INTEGER NOT NULL DEFAULT 0,
    service_endpoint TEXT,
    secret_ref       TEXT,
    state            TEXT NOT NULL DEFAULT 'provisioning', -- provisioning|ready|degraded|deleting
    metadata         TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata)),
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

-- A Debezium CDC stream keeping the edge DB in sync with the source (WAL/binlog -> apply).
CREATE TABLE IF NOT EXISTS cdc_streams (
    id               TEXT PRIMARY KEY,
    tenant_id        TEXT NOT NULL DEFAULT 'global',
    plan_id          TEXT,
    engine           TEXT NOT NULL,                       -- postgres | mysql
    connect_name     TEXT,                                -- KafkaConnect / Redpanda Connect name
    connector_name   TEXT,                                -- Debezium connector name
    topic_prefix     TEXT,
    state            TEXT NOT NULL DEFAULT 'starting',    -- starting|snapshotting|streaming|paused|stopped|error
    lag_bytes        INTEGER NOT NULL DEFAULT 0,
    lag_seconds      INTEGER NOT NULL DEFAULT 0,
    last_source_lsn  TEXT,
    last_applied_lsn TEXT,
    events_total     INTEGER NOT NULL DEFAULT 0,
    lag_updated_at   TEXT,
    created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

-- A migration plan: source -> edge, with assessment, CDC, and cutover state.
CREATE TABLE IF NOT EXISTS migration_plans (
    id                 TEXT PRIMARY KEY,
    tenant_id          TEXT NOT NULL DEFAULT 'global',
    name               TEXT NOT NULL,
    source_id          TEXT NOT NULL REFERENCES migration_sources(id) ON DELETE CASCADE,
    edge_cluster_id    TEXT REFERENCES edge_db_clusters(id) ON DELETE SET NULL,
    cdc_stream_id      TEXT REFERENCES cdc_streams(id) ON DELETE SET NULL,
    readiness_score    INTEGER NOT NULL DEFAULT 0,
    assessment         TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(assessment)),
    rollback_window_secs INTEGER NOT NULL DEFAULT 259200, -- 72h default rollback window
    cutover_at         TEXT,
    state              TEXT NOT NULL DEFAULT 'draft',
    -- draft|discovered|assessed|provisioning|provisioned|full_loading|loaded|
    -- cdc_streaming|validating|validated|cutover_pending|cutover_in_progress|
    -- cutover_complete|completed|rolled_back|failed
    created_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

-- A validation run comparing source vs edge (row counts, checksums, schema diff).
CREATE TABLE IF NOT EXISTS validation_runs (
    id                TEXT PRIMARY KEY,
    tenant_id         TEXT NOT NULL DEFAULT 'global',
    plan_id           TEXT NOT NULL REFERENCES migration_plans(id) ON DELETE CASCADE,
    kind              TEXT NOT NULL DEFAULT 'rowcount',    -- rowcount|checksum|schema-diff|final
    state             TEXT NOT NULL DEFAULT 'running',     -- running|passed|failed
    tables_total      INTEGER NOT NULL DEFAULT 0,
    tables_mismatched INTEGER NOT NULL DEFAULT 0,
    summary           TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(summary)),
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    completed_at      TEXT
);

-- A cutover: freeze source, drain CDC lag, switch endpoint, open a rollback window.
CREATE TABLE IF NOT EXISTS cutovers (
    id                TEXT PRIMARY KEY,
    tenant_id         TEXT NOT NULL DEFAULT 'global',
    plan_id           TEXT NOT NULL REFERENCES migration_plans(id) ON DELETE CASCADE,
    state             TEXT NOT NULL DEFAULT 'freezing',    -- freezing|draining|switching|complete|rolled_back|failed
    from_endpoint     TEXT,
    to_endpoint       TEXT,
    drain_deadline    TEXT,
    rollback_deadline TEXT,
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    completed_at      TEXT
);

CREATE INDEX IF NOT EXISTS idx_plans_source ON migration_plans(source_id);
CREATE INDEX IF NOT EXISTS idx_edge_plan ON edge_db_clusters(plan_id);
CREATE INDEX IF NOT EXISTS idx_cdc_plan ON cdc_streams(plan_id);
CREATE INDEX IF NOT EXISTS idx_validations_plan ON validation_runs(plan_id);
CREATE INDEX IF NOT EXISTS idx_cutovers_plan ON cutovers(plan_id);
