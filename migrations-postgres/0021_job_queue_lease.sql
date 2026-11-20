-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Durable job queue: track which worker claimed a job and when, so a multi-worker / multi-replica
-- deployment can reclaim stale `running` rows without failing every interrupted job on restart.
-- The in-memory mpsc channel remains a fast wake-up; the DB is the source of truth.
ALTER TABLE storage_jobs ADD COLUMN locked_by TEXT;
ALTER TABLE storage_jobs ADD COLUMN locked_at TEXT;
