-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Latest normalized Ceph metrics scraped from the mgr Prometheus module (PDF §15.1).
-- We keep only the latest value per (name, labels) — not a full time series (that belongs in
-- Prometheus itself); Atlas stores a curated snapshot for its summary views + alert rules.

CREATE TABLE IF NOT EXISTS storage_metrics (
    name       TEXT NOT NULL,
    labels     TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(labels)),
    value      REAL NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    UNIQUE (name, labels)
);

CREATE INDEX IF NOT EXISTS idx_metrics_name ON storage_metrics(name);
