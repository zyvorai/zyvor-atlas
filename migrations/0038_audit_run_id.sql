-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: Apache-2.0
-- Agent run correlation id: an optional id an agent supplies (X-Atlas-Run-Id on its MCP calls) that is
-- stored on every audit row the call causes, so one run can be joined across the gateway audit trail
-- and the atlas_data audit trail (docs/MCP.md). NULL for every row not made under a run id.
ALTER TABLE storage_audit_logs ADD COLUMN run_id TEXT;
CREATE INDEX IF NOT EXISTS idx_audit_run_id ON storage_audit_logs(run_id);
