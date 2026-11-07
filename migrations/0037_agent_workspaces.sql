-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: Apache-2.0
-- Agent workspaces: a disposable, isolated copy-on-write clone of an atlas-native volume or filesystem
-- for one agent run (docs/NATIVE_FS.md). `base_snapshot_id` is the point the clone was taken from (a
-- reset re-clones from it); `owns_base_snapshot` is 1 when Atlas took that snapshot itself and must
-- delete it with the workspace. Times are Unix seconds so expiry compares the same on both dialects.
CREATE TABLE IF NOT EXISTS agent_workspaces (
    id                 TEXT PRIMARY KEY,
    tenant_id          TEXT NOT NULL,
    owner              TEXT NOT NULL,
    name               TEXT NOT NULL,
    source_volume_id   TEXT NOT NULL,
    base_snapshot_id   TEXT NOT NULL,
    owns_base_snapshot BIGINT NOT NULL DEFAULT 0,
    volume_id          TEXT NOT NULL,
    max_bytes          BIGINT NOT NULL DEFAULT 0,
    state              TEXT NOT NULL,
    reset_count        BIGINT NOT NULL DEFAULT 0,
    last_error         TEXT,
    created_at_unix    BIGINT NOT NULL,
    expires_at_unix    BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_agent_workspaces_tenant ON agent_workspaces (tenant_id);
CREATE INDEX IF NOT EXISTS idx_agent_workspaces_expiry ON agent_workspaces (expires_at_unix);
