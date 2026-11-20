-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Day-2 HA: a DB-backed leader lease so the periodic workers (monitor, scheduler, reconciler, …) run
-- on exactly one replica. On single-replica SQLite this instance always holds it (no behaviour
-- change); the mechanism is what makes a future multi-replica / Postgres-backed deployment safe
-- (only the leader schedules, so jobs aren't double-fired).
CREATE TABLE IF NOT EXISTS leader_lease (
    name       TEXT PRIMARY KEY,     -- lease name (e.g. 'workers')
    holder     TEXT NOT NULL,        -- current holder's instance id
    expires_at TEXT NOT NULL         -- lease expiry (RFC3339 UTC)
);
