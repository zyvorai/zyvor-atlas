-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Extend protection schedules to also run periodic backups to RGW (PDF §12.3 "daily backup"), not
-- just snapshots. `kind` selects the action; backups additionally need a target bucket + mode.

ALTER TABLE snapshot_schedules ADD COLUMN kind TEXT NOT NULL DEFAULT 'snapshot';  -- snapshot | backup
ALTER TABLE snapshot_schedules ADD COLUMN bucket_id TEXT;                          -- backup target
ALTER TABLE snapshot_schedules ADD COLUMN mode TEXT NOT NULL DEFAULT 'manifest';  -- backup mode
