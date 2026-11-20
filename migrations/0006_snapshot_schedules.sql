-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Protection schedules (PDF §12.3 "hourly snapshots"): a background scheduler snapshots a volume on
-- a fixed interval and prunes the scheduled snapshots to a retention count.

CREATE TABLE IF NOT EXISTS snapshot_schedules (
    id            TEXT PRIMARY KEY,
    tenant_id     TEXT NOT NULL DEFAULT 'global',
    volume_id     TEXT NOT NULL REFERENCES storage_volumes(id) ON DELETE CASCADE,
    interval_secs INTEGER NOT NULL,             -- how often to snapshot
    keep          INTEGER NOT NULL DEFAULT 0,   -- retain the newest N scheduled snapshots (0 = all)
    enabled       INTEGER NOT NULL DEFAULT 1,
    last_run_at   TEXT,
    next_run_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_snapshot_schedules_due
    ON snapshot_schedules(enabled, next_run_at);
