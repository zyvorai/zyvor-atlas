-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Widen every byte-count/capacity column from the Postgres-dialect INTEGER (4 bytes, max ~2.1GB)
-- to BIGINT (8 bytes) — found live via crates/atlas-gateway/tests/*.rs's new dual-backend run
-- (docs/HA.md): a real NFS/ZFS discovery pass failed with "integer out of range" persisting a
-- multi-TB pool capacity. SQLite needed no equivalent fix — its own INTEGER column type is
-- *always* a dynamically-typed 8-byte signed integer regardless of the declared name (see the
-- type-mapping convention documented at the top of migrations/0001_init.sql), so this bug only
-- ever existed on the Postgres side of the schema fork, never exercised until real data flowed
-- through it here. ALTER COLUMN ... TYPE BIGINT is safe and (for INTEGER -> BIGINT specifically)
-- fast on modern Postgres — widening never truncates existing data.
ALTER TABLE storage_clusters
    ALTER COLUMN raw_capacity_bytes TYPE BIGINT,
    ALTER COLUMN used_capacity_bytes TYPE BIGINT,
    ALTER COLUMN available_capacity_bytes TYPE BIGINT;

ALTER TABLE storage_pools
    ALTER COLUMN used_bytes TYPE BIGINT,
    ALTER COLUMN max_bytes TYPE BIGINT;

ALTER TABLE storage_osds
    ALTER COLUMN used_bytes TYPE BIGINT,
    ALTER COLUMN capacity_bytes TYPE BIGINT;

ALTER TABLE storage_volumes
    ALTER COLUMN size_bytes TYPE BIGINT,
    ALTER COLUMN used_bytes TYPE BIGINT;

ALTER TABLE storage_tenant_quotas
    ALTER COLUMN max_bytes TYPE BIGINT;

ALTER TABLE metrics_history
    ALTER COLUMN raw_capacity_bytes TYPE BIGINT,
    ALTER COLUMN used_capacity_bytes TYPE BIGINT;

ALTER TABLE edge_db_clusters
    ALTER COLUMN size_bytes TYPE BIGINT;

ALTER TABLE cdc_streams
    ALTER COLUMN lag_bytes TYPE BIGINT;

ALTER TABLE object_migrations
    ALTER COLUMN bytes_total TYPE BIGINT,
    ALTER COLUMN bytes_done TYPE BIGINT;
