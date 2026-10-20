-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: Apache-2.0
-- Case-insensitive username uniqueness. The SQLite schema gets it from `COLLATE NOCASE` on the
-- primary key; Postgres' plain TEXT primary key allowed `Admin` and `admin` to coexist while every
-- lookup in crates/atlas-inventory/src/users.rs matches on `lower(username)`, so login/update/delete
-- could hit either row. Enforce it with a unique expression index.
--
-- Pre-check: if a database already holds case-colliding usernames, fail with an explicit message
-- instead of picking a row to delete or rename. Resolve by hand, then restart, e.g.
--   DELETE FROM console_users WHERE username = '<the duplicate to drop>';
DO $$
DECLARE
    dups text;
BEGIN
    SELECT string_agg(names, '; ') INTO dups FROM (
        SELECT string_agg(username, ', ' ORDER BY username) AS names
        FROM console_users
        GROUP BY lower(username)
        HAVING count(*) > 1
    ) d;
    IF dups IS NOT NULL THEN
        RAISE EXCEPTION 'console_users has usernames that differ only by case (%); delete or rename the duplicates, then restart', dups;
    END IF;
END
$$;
CREATE UNIQUE INDEX IF NOT EXISTS uq_console_users_lower_username ON console_users (lower(username));
