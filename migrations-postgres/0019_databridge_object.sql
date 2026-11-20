-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Zyvor DataBridge — object-storage migration (cloud object store -> Ceph RGW).
-- Multi-cloud: any S3-protocol source (AWS S3, Google Cloud Storage via S3-interop,
-- MinIO/Wasabi/other S3-compatible) copies today; Azure Blob / VMware are provider
-- values reserved for their connectors (mirrors the DB side's cloud=rds|aurora|cloudsql).
-- Credentials are NEVER stored here — only a reference to a k8s Secret resolved at run time.

CREATE TABLE IF NOT EXISTS object_migrations (
    id                TEXT PRIMARY KEY,
    tenant_id         TEXT NOT NULL DEFAULT 'global',
    name              TEXT NOT NULL,

    -- source object store
    source_provider   TEXT NOT NULL DEFAULT 's3-compatible',  -- aws | gcs | s3-compatible | azure-blob | vmware
    source_endpoint   TEXT NOT NULL,
    source_region     TEXT NOT NULL DEFAULT 'us-east-1',
    source_bucket     TEXT NOT NULL,
    source_prefix     TEXT,
    source_secret_ref TEXT,                                    -- k8s Secret {access_key,secret_key}; never the creds

    -- destination object store (Ceph RGW, or any S3-compatible target)
    dest_provider     TEXT NOT NULL DEFAULT 's3-compatible',
    dest_endpoint     TEXT NOT NULL,
    dest_region       TEXT NOT NULL DEFAULT 'us-east-1',
    dest_bucket       TEXT NOT NULL,
    dest_secret_ref   TEXT,

    secret_namespace  TEXT NOT NULL DEFAULT 'zyvor-databridge',
    mode              TEXT NOT NULL DEFAULT 'incremental',     -- full | incremental
    state             TEXT NOT NULL DEFAULT 'created',         -- created|planning|copying|verifying|completed|failed

    objects_total     INTEGER NOT NULL DEFAULT 0,
    objects_done      INTEGER NOT NULL DEFAULT 0,
    bytes_total       INTEGER NOT NULL DEFAULT 0,
    bytes_done        INTEGER NOT NULL DEFAULT 0,
    verified          INTEGER NOT NULL DEFAULT 0,              -- 0/1

    last_error        TEXT,
    job_id            TEXT,
    created_at        TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    updated_at        TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
);

CREATE INDEX IF NOT EXISTS idx_object_migrations_state ON object_migrations (state, created_at);
CREATE INDEX IF NOT EXISTS idx_object_migrations_tenant ON object_migrations (tenant_id, created_at);
