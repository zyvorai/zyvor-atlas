-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Widen every REAL column to DOUBLE PRECISION — found live via crates/atlas-gateway/tests/*.rs's
-- dual-backend run (docs/HA.md): sqlx decoded a Rust `f64` bind against a Postgres `REAL` (4-byte
-- float4) column and failed with "mismatched types; Rust type `f64` is not compatible with SQL
-- type `REAL`". SQLite needed no equivalent fix — its own REAL column type is *always* an 8-byte
-- IEEE double regardless of the declared name (same "declared name isn't the real width" trap as
-- INTEGER vs BIGINT, fixed for the integer side in migration 0032), so this bug only ever existed
-- on the Postgres side of the schema fork.
ALTER TABLE storage_metrics
    ALTER COLUMN value TYPE DOUBLE PRECISION;

ALTER TABLE metrics_history
    ALTER COLUMN read_bytes TYPE DOUBLE PRECISION,
    ALTER COLUMN write_bytes TYPE DOUBLE PRECISION,
    ALTER COLUMN read_ops TYPE DOUBLE PRECISION,
    ALTER COLUMN write_ops TYPE DOUBLE PRECISION;

ALTER TABLE object_migrations
    ALTER COLUMN throughput_mbps TYPE DOUBLE PRECISION;
