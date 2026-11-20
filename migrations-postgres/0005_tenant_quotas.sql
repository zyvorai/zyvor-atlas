-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Per-tenant storage quotas (PDF §14: multi-tenancy). A tenant's total provisioned volume capacity
-- and volume count are capped here; the gateway rejects a create that would exceed either limit.
-- A value of 0 means "unlimited" for that dimension.

CREATE TABLE IF NOT EXISTS storage_tenant_quotas (
    tenant_id    TEXT PRIMARY KEY,
    max_bytes    INTEGER NOT NULL DEFAULT 0,   -- 0 = unlimited total provisioned volume bytes
    max_volumes  INTEGER NOT NULL DEFAULT 0,   -- 0 = unlimited volume count
    created_at   TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    updated_at   TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
);
