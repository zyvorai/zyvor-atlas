-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: Apache-2.0
-- Single-use record of MCP write confirmation tokens (docs/MCP.md). A confirmation token is a signed JWT whose
-- `jti` is inserted here when it is redeemed; the PRIMARY KEY turns a second redemption (a replay, a retry racing
-- the first call, another gateway replica) into a unique violation, which is how "already used" is decided. Rows
-- are only needed until the token's own expiry, so the gateway prunes rows whose `exp` (unix seconds) is long past.
CREATE TABLE IF NOT EXISTS mcp_confirm_used (
    jti TEXT PRIMARY KEY,
    exp BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mcp_confirm_used_exp ON mcp_confirm_used(exp);
