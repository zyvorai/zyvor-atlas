-- Copyright (c) 2026 ZyvorAI Labs Private Limited.
-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
-- `JobEngine::enqueue` (atlas-jobs/src/engine.rs) deduplicates on `idempotency_key` by SELECTing
-- for an existing job and, if none is found, INSERTing a new row (PDF §17.4) — a classic
-- check-then-act race: two concurrent submissions of the same key (a client retry after a timeout,
-- or two gateway requests) can both see "no existing job" before either commits its INSERT, landing
-- two `storage_jobs` rows with the same idempotency_key and defeating the dedup guarantee callers
-- rely on (e.g. `POST /volumes`'s deterministic per-tenant/name/size key). A partial UNIQUE index
-- (NULL keys — the common case for non-idempotent jobs — are exempt, matching normal SQL UNIQUE
-- semantics) turns the second concurrent INSERT into a constraint violation instead of a silent
-- duplicate; `enqueue` catches that and returns the winner's existing job.
CREATE UNIQUE INDEX IF NOT EXISTS idx_storage_jobs_idempotency_key
    ON storage_jobs(idempotency_key) WHERE idempotency_key IS NOT NULL;
