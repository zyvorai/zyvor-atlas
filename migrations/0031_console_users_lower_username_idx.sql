-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- crates/atlas-inventory/src/users.rs looks up/orders by `lower(username)` (portable replacement
-- for SQLite's `COLLATE NOCASE`, which Postgres has no equivalent of on a plain TEXT column — see
-- migrations-postgres/0025_console_users.sql). An expression index lets both backends use an index
-- scan for that lookup instead of a full table scan, same as the COLLATE NOCASE primary key did
-- for the old `WHERE username = ?` form.
CREATE INDEX IF NOT EXISTS idx_console_users_lower_username ON console_users (lower(username));
