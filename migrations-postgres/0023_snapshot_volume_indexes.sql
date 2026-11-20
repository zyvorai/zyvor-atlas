-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Hot-path read indexes: both tables are looked up by `volume_id` on every
-- "list snapshots/schedules for this volume" request (atlas-inventory
-- snapshots::list_snapshots / schedules::list), and `volume_id` is also the
-- ON DELETE CASCADE key scanned whenever a volume is deleted. Neither column
-- had an index, forcing a full table scan on both paths as either table grows.
CREATE INDEX IF NOT EXISTS idx_snapshots_volume ON storage_snapshots(volume_id);
CREATE INDEX IF NOT EXISTS idx_snapshot_schedules_volume ON snapshot_schedules(volume_id);
