-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Day-2 DataBridge: CDC self-heal. Track how many times a stream's connectors have been restarted so
-- the reconciler can auto-restart a stalled stream a bounded number of times before giving up (and
-- letting the CDC-error alert fire).
ALTER TABLE cdc_streams ADD COLUMN restart_count INTEGER NOT NULL DEFAULT 0;
