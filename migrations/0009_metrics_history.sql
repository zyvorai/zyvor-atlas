-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Server-side metrics time-series: one row per sampler tick, so capacity/IO/job trends
-- survive gateway restarts and page reloads (the UI previously kept these only in-memory).
CREATE TABLE IF NOT EXISTS metrics_history (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    ts                   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    raw_capacity_bytes   INTEGER NOT NULL DEFAULT 0,
    used_capacity_bytes  INTEGER NOT NULL DEFAULT 0,
    volumes              INTEGER NOT NULL DEFAULT 0,
    snapshots            INTEGER NOT NULL DEFAULT 0,
    read_bytes           REAL    NOT NULL DEFAULT 0,
    write_bytes          REAL    NOT NULL DEFAULT 0,
    read_ops             REAL    NOT NULL DEFAULT 0,
    write_ops            REAL    NOT NULL DEFAULT 0,
    jobs_running         INTEGER NOT NULL DEFAULT 0,
    alerts_open          INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_metrics_history_ts ON metrics_history(ts);
