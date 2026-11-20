-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Object-migration tuning + throughput: per-migration concurrency / part size, when the
-- copy started, and the live transfer rate.

ALTER TABLE object_migrations ADD COLUMN concurrency INTEGER;         -- objects copied at once (NULL = env default)
ALTER TABLE object_migrations ADD COLUMN part_size_mb INTEGER;        -- multipart chunk MiB (NULL = env default)
ALTER TABLE object_migrations ADD COLUMN throughput_mbps REAL NOT NULL DEFAULT 0;
ALTER TABLE object_migrations ADD COLUMN started_at TEXT;             -- set when state -> copying
