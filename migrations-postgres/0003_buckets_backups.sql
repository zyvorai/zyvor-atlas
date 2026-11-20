-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- RGW/S3 object buckets + backups (PDF §9.9, §16).

CREATE TABLE IF NOT EXISTS storage_buckets (
    id           TEXT PRIMARY KEY,
    tenant_id    TEXT NOT NULL DEFAULT 'global',
    backend_id   TEXT REFERENCES storage_backends(id) ON DELETE SET NULL,
    name         TEXT NOT NULL,
    bucket_name  TEXT,                                   -- actual RGW bucket name (OBC may generate)
    endpoint     TEXT,
    region       TEXT,
    secret_ref   TEXT,                                   -- k8s Secret name; never the keys
    obc_name     TEXT,                                   -- backing ObjectBucketClaim
    namespace    TEXT,
    state        TEXT NOT NULL DEFAULT 'pending',
    metadata     TEXT NOT NULL DEFAULT '{}' CHECK ((metadata)::json IS NOT NULL),
    created_at   TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
);

CREATE TABLE IF NOT EXISTS storage_backups (
    id            TEXT PRIMARY KEY,
    tenant_id     TEXT NOT NULL DEFAULT 'global',
    volume_id     TEXT NOT NULL,
    snapshot_id   TEXT,
    bucket_id     TEXT NOT NULL REFERENCES storage_buckets(id) ON DELETE CASCADE,
    object_key    TEXT NOT NULL,                         -- S3 key of the backup manifest
    format        TEXT NOT NULL DEFAULT 'manifest-v1',
    checksum      TEXT,
    state         TEXT NOT NULL DEFAULT 'pending',
    manifest      TEXT NOT NULL DEFAULT '{}' CHECK ((manifest)::json IS NOT NULL),
    created_at    TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
);

CREATE INDEX IF NOT EXISTS idx_backups_volume ON storage_backups(volume_id);
CREATE INDEX IF NOT EXISTS idx_backups_bucket ON storage_backups(bucket_id);
