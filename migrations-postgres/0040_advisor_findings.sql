-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: Apache-2.0
-- One row per Ops Advisor run (`POST /ai/advisor`). The advisor still cannot execute a runbook;
-- `GET /ai/advisor/findings` reads these rows back. `payload` is the evidence, actions and warnings
-- as JSON text (`can_execute` is recorded false).
CREATE TABLE IF NOT EXISTS advisor_findings (
    id          TEXT PRIMARY KEY,
    tenant_id   TEXT NOT NULL,
    actor_id    TEXT NOT NULL,
    question    TEXT NOT NULL,
    mode        TEXT NOT NULL,
    risk_score  BIGINT NOT NULL,
    risk_level  TEXT NOT NULL,
    summary     TEXT NOT NULL,
    payload     TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
);
CREATE INDEX IF NOT EXISTS idx_advisor_findings_created ON advisor_findings(created_at);
