-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: Apache-2.0
-- Case-insensitive username uniqueness. SQLite already has it (the primary key is COLLATE NOCASE);
-- this index keeps both dialects uniform and mirrors migrations-postgres/0036, where it is the
-- actual enforcement (see the note there). Cannot fail here: NOCASE already forbids duplicates.
CREATE UNIQUE INDEX IF NOT EXISTS uq_console_users_lower_username ON console_users (lower(username));
