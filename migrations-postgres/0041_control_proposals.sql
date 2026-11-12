-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: Apache-2.0
-- Two-person proposals. `kind` is `snapshot_unlock` (WORM: a second admin clears a retention lock)
-- or `alert_ack` (a second operator acknowledges an alert). `state` moves pending → confirming →
-- confirmed; a failed confirm returns the row to pending. `expires_unix` is the deadline.
CREATE TABLE IF NOT EXISTS control_proposals (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    subject_id    TEXT NOT NULL,
    tenant_id     TEXT NOT NULL,
    proposed_by   TEXT NOT NULL,
    reason        TEXT NOT NULL DEFAULT '',
    created_at    TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')),
    expires_unix  BIGINT NOT NULL,
    state         TEXT NOT NULL,
    confirmed_by  TEXT
);
CREATE INDEX IF NOT EXISTS idx_control_proposals_subject
    ON control_proposals(kind, subject_id, state);
