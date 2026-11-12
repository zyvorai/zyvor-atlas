-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: Apache-2.0
-- Intent a product owner asked Atlas to record. Atlas does not call the product.
CREATE TABLE IF NOT EXISTS owner_workflows (
    id          TEXT PRIMARY KEY,
    tenant_id   TEXT NOT NULL,
    owner_id    TEXT NOT NULL,
    intent      TEXT NOT NULL,
    note        TEXT NOT NULL DEFAULT '',
    actor_id    TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_owner_workflows_owner
    ON owner_workflows(owner_id, created_at);
