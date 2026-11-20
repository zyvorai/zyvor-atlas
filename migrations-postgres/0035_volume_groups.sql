-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Multi-image consistency groups (RBD groups, docs/DR.md) and the pool-level mirroring mode Atlas
-- last applied. volume_groups maps 1:1 to `rbd group` <pool>/<name>; snapshots are group snapshots
-- (one crash-consistent point across every member). They stay on this cluster: rbd-mirror does not
-- replicate group snapshots.
CREATE TABLE IF NOT EXISTS volume_groups (
    id         TEXT PRIMARY KEY,
    tenant_id  TEXT NOT NULL DEFAULT 'global',
    name       TEXT NOT NULL,
    pool       TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    UNIQUE(pool, name)
);

CREATE TABLE IF NOT EXISTS volume_group_members (
    group_id  TEXT NOT NULL,
    volume_id TEXT NOT NULL,
    pool      TEXT NOT NULL,
    image     TEXT NOT NULL,
    PRIMARY KEY (group_id, volume_id)
);

CREATE TABLE IF NOT EXISTS volume_group_snapshots (
    id         TEXT PRIMARY KEY,
    group_id   TEXT NOT NULL,
    name       TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    UNIQUE(group_id, name)
);

CREATE TABLE IF NOT EXISTS dr_pool_mirroring (
    pool       TEXT PRIMARY KEY,
    mode       TEXT NOT NULL,                              -- image | pool
    updated_at TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
);
