-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- DR hardening: track last failover / last error and optional force-promote metadata so drills and
-- failed real `rbd mirror` ops are visible in the catalog (see docs/DR.md).
ALTER TABLE dr_mirrors ADD COLUMN last_failover_at TEXT;
ALTER TABLE dr_mirrors ADD COLUMN last_error TEXT;
ALTER TABLE dr_mirrors ADD COLUMN force_promoted INTEGER NOT NULL DEFAULT 0;
