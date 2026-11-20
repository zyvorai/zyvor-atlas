-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Fake-driver-mode catalog for direct RBD image snapshots (bypassing CSI). Real mode has no need
-- for this — truth lives in Ceph itself via `rbd snap ls` — but fake mode has no `rbd` CLI and
-- previously had no way to remember a snapshot it had just "created", so the panel always looked
-- empty even right after a successful create.
CREATE TABLE IF NOT EXISTS rbd_snapshots (
    pool TEXT NOT NULL,
    image TEXT NOT NULL,
    snap TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (pool, image, snap)
);
