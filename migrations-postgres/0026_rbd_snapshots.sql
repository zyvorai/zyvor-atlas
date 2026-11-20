-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
CREATE TABLE IF NOT EXISTS rbd_snapshots (
    pool       TEXT NOT NULL,
    image      TEXT NOT NULL,
    snap       TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    PRIMARY KEY (pool, image, snap)
);
