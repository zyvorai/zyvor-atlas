-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Per-tenant policy overrides (PDF §14): a tenant can remap an intent (e.g. "database") to a
-- different StorageClass / access+volume mode than the built-in atlas-policy catalog. When a create
-- names an intent and the tenant has an override for it, the override wins (unless the request pins
-- an explicit StorageClass).

CREATE TABLE IF NOT EXISTS tenant_policies (
    tenant_id     TEXT NOT NULL,
    intent        TEXT NOT NULL,
    storage_class TEXT NOT NULL,
    access_mode   TEXT NOT NULL DEFAULT 'ReadWriteOnce',
    volume_mode   TEXT NOT NULL DEFAULT 'Filesystem',
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (tenant_id, intent)
);
