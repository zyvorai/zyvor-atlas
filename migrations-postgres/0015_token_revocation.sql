-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Day-2 governance: a deny-list of revoked JWT ids (jti). The auth middleware rejects any presented
-- token whose jti is here, giving a kill switch for a leaked/rotated service-account credential
-- (previously a token was valid until its TTL expired, with no way to invalidate it early).
CREATE TABLE IF NOT EXISTS revoked_tokens (
    jti        TEXT PRIMARY KEY,
    revoked_by TEXT,
    revoked_at TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
);
