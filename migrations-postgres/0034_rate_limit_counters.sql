-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Cross-replica rate-limit sync (docs/HA.md). See migrations/0034_rate_limit_counters.sql for the
-- full design note. `window_minute`/`count` deliberately stay plain INTEGER (not BIGINT, unlike
-- migration 0032's byte-capacity columns) — both are guaranteed small (a Unix-minute counter and a
-- per-replica-per-minute request count bounded by the configured rpm), and keeping them INTEGER
-- means SUM(count) returns Postgres BIGINT rather than NUMERIC, sidestepping the same
-- sqlx::Any-can't-decode-NUMERIC issue migration 0032/0033 had to fix for the columns that
-- genuinely needed 64-bit range.
CREATE TABLE IF NOT EXISTS rate_limit_counters (
    actor_id      TEXT NOT NULL,
    window_minute INTEGER NOT NULL,
    replica_id    TEXT NOT NULL,
    count         INTEGER NOT NULL DEFAULT 0,
    updated_at    TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    PRIMARY KEY (actor_id, window_minute, replica_id)
);
CREATE INDEX IF NOT EXISTS idx_rate_limit_counters_window ON rate_limit_counters(window_minute);
