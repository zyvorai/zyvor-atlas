-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Day-2 durability: bounded retry-with-backoff for the async job engine. Recovery on restart
-- (re-enqueue queued jobs, fail interrupted ones) uses the existing state column; these columns add
-- opt-in retry accounting so a transient failure can be re-attempted instead of going terminal.
ALTER TABLE storage_jobs ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE storage_jobs ADD COLUMN max_retries INTEGER NOT NULL DEFAULT 0;
ALTER TABLE storage_jobs ADD COLUMN next_attempt_at TEXT;
