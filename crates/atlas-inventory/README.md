<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# atlas-inventory

The normalized inventory read model + audit log, backed by SQLite or Postgres.

- **`connect` / `migrate`** — open a backend-agnostic `sqlx::AnyPool` (SQLite or Postgres, chosen
  by `database_url`'s scheme — no Cargo feature flag) and run the embedded migrations from
  `migrations/` (SQLite) or `migrations-postgres/` (Postgres), kept in lockstep by
  `scripts/check-migrations-parity.sh` (wired into CI). SQLite-only pragmas (WAL, foreign keys,
  `busy_timeout`) apply via an `after_connect` hook, skipped for Postgres. See `docs/HA.md`.
- **Writes** — `upsert_backend`, `upsert_discovery` (one transaction: cluster → pools → osds →
  volumes, with enum↔TEXT mapping).
- **Reads** — `list_backends`, `list_clusters`, `get_cluster`, `cluster_health`, `list_pools`,
  `list_osds`, `list_volumes`, `get_volume`, `metrics_summary`. These return `atlas-api-types`
  DTOs and power the gateway's read endpoints.
- **`audit`** — `record(...)` appends to `storage_audit_logs`; `count_for_action` (used in tests).

Schema: `migrations/0001_init.sql` (SQLite). Postgres mirror: `migrations-postgres/`.
Uses runtime `sqlx::query` with `$N` placeholders against `sqlx::AnyPool` (no compile-time
`DATABASE_URL` needed, and the same query text runs on either backend).
