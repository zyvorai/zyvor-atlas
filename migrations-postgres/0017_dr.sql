-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Day-2 cross-cluster DR (RBD mirroring). SCAFFOLDING: the control-plane state + API are here; the
-- real `rbd mirror` operations run as jobs and are UNVERIFIED until exercised against a live second
-- Ceph cluster (see docs). dr_peers records a mirroring peer (bootstrap token via a k8s Secret ref);
-- dr_mirrors tracks each mirrored RBD image's role (primary/secondary) + replication state + RPO.
CREATE TABLE IF NOT EXISTS dr_peers (
    id                   TEXT PRIMARY KEY,
    name                 TEXT NOT NULL,
    cluster_fsid         TEXT,
    direction            TEXT NOT NULL DEFAULT 'rx-tx',    -- rx-tx | rx-only
    bootstrap_secret_ref TEXT,                             -- k8s Secret holding the peer bootstrap token
    state                TEXT NOT NULL DEFAULT 'registered',
    created_at           TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
);

CREATE TABLE IF NOT EXISTS dr_mirrors (
    id          TEXT PRIMARY KEY,
    tenant_id   TEXT NOT NULL DEFAULT 'global',
    volume_id   TEXT,
    pool        TEXT NOT NULL,
    image       TEXT NOT NULL,
    peer_id     TEXT,
    mode        TEXT NOT NULL DEFAULT 'snapshot',          -- snapshot | journal
    role        TEXT NOT NULL DEFAULT 'primary',           -- primary | secondary
    state       TEXT NOT NULL DEFAULT 'enabling',          -- enabling|enabled|promoting|demoting|disabled|error
    rpo_seconds INTEGER,
    updated_at  TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    created_at  TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    UNIQUE(pool, image)
);
