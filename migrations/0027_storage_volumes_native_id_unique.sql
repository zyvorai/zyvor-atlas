-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- Same class of race as migrations/0024 (idempotency_key): the direct-RBD create/clone routes
-- check "does this pool/name already exist?" before enqueueing, but that check is not atomic with
-- the job's later INSERT — two concurrent requests for the same name can both see "doesn't exist"
-- before either commits, landing two storage_volumes rows (different id, same backend_native_id)
-- for what should be one physical RBD image. A partial UNIQUE index (NULL native ids — volumes a
-- driver hasn't resolved a backend identity for yet — are exempt, matching normal SQL UNIQUE
-- semantics) turns the second concurrent write into a constraint violation instead of a silent
-- duplicate catalog row.
CREATE UNIQUE INDEX IF NOT EXISTS idx_storage_volumes_native_id
    ON storage_volumes(backend_native_id) WHERE backend_native_id IS NOT NULL;
