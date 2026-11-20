-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Cross-replica rate-limit sync (docs/HA.md). Each gateway replica keeps its own fast, in-process,
-- fully synchronous per-actor counter (crates/atlas-gateway/src/state.rs::RateLimiter — deliberately
-- never touches the database directly, since it must stay callable from the gRPC path's synchronous
-- tonic::Interceptor without risking blocking the Tokio runtime). A separate periodic background
-- task (spawn_rate_limit_sync, async, like every other periodic worker in this codebase) writes
-- each replica's own current-window count here, then reads back the summed cluster-wide total per
-- actor to decide whether that actor is over budget *cluster-wide* — eventually consistent within
-- one sync interval (a few seconds), not a hard real-time guarantee, which is the normal tradeoff
-- for a governance/abuse-prevention control rather than a security boundary.
CREATE TABLE IF NOT EXISTS rate_limit_counters (
    actor_id      TEXT NOT NULL,
    window_minute INTEGER NOT NULL,
    replica_id    TEXT NOT NULL,
    count         INTEGER NOT NULL DEFAULT 0,
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (actor_id, window_minute, replica_id)
);
CREATE INDEX IF NOT EXISTS idx_rate_limit_counters_window ON rate_limit_counters(window_minute);
