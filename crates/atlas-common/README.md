<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# atlas-common

Shared primitives for every Atlas crate.

- **`config`** — `Config::from_env()` (loads `.env` via `dotenvy`); secret-redacting `Debug`;
  `CephDriverMode` (`real`/`fake`); `jwt_secret_is_weak()`.
- **`error`** — `AppError` (`thiserror`) with an axum `IntoResponse` mapping to sanitized JSON
  error envelopes; `AppResult<T>`. Converts from `anyhow::Error`.
- **`ids`** — resource-id helpers with the PDF §10.1 prefix scheme (`bkd_`, `cls_`, `pool_`,
  `vol_`, `snap_`, `job_`, …) plus `stable_id()` (deterministic FNV-1a for idempotent upserts).
- **`init_tracing()`** — one-time `tracing` setup honoring `RUST_LOG`.

No database or backend dependencies — safe to use anywhere.
