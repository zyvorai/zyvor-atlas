-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Day-2 alerting maturity: manual alert lifecycle. Acknowledge records that an operator has seen an
-- alert; silence suppresses webhook notification until a deadline (the condition is still tracked).
ALTER TABLE storage_alerts ADD COLUMN acknowledged_at TEXT;
ALTER TABLE storage_alerts ADD COLUMN acknowledged_by TEXT;
ALTER TABLE storage_alerts ADD COLUMN silenced_until TEXT;
