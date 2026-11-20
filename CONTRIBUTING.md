<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Contributing to Atlas

Atlas is licensed under the Apache License, Version 2.0 — see
[`docs/LICENSING.md`](docs/LICENSING.md).
All participation is governed by the [Code of Conduct](CODE_OF_CONDUCT.md).

## Conventions

- **License header** on every source file (enforced by `make headers` /
  `scripts/check-license-headers.sh`):
  ```
  // Copyright (c) 2026 ZyvorAI Labs Private Limited.
  // SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
  ```
  (`#` / `<!-- -->` / `/* */` / `--` variants by file type). Workspace Cargo
  `license = "Apache-2.0"`.
- **Stack**: axum 0.8, `sqlx` (SQLite), `thiserror` 2.0 + `anyhow`, `tracing`, `kube-rs`.
  Match the surrounding monorepo services (`ragnarok`, `machina`).
- **Errors**: return `atlas_common::AppError` from handlers; it maps to HTTP via `IntoResponse`.
  Library crates return `anyhow::Result` / typed errors and convert at the edge.
- **Ceph/rbd commands**: build arguments as **arrays only**, never string concatenation
  (`ceph_cmd(&["df", "detail"])`). Always `--format json`.
- **Secrets**: store *references*, never values. Never print a keyring/secret to logs or output.
- Run `make headers && make dev` (license headers + clippy `-D warnings` + tests) before committing.

## Adding a REST endpoint

1. Add the handler in `crates/atlas-gateway/src/routes.rs` (return `AppResult<Json<...>>`).
2. Wire the route in `router()`.
3. Read data via `atlas-inventory` (SQLite) or a driver; don't touch backends directly from the
   handler.
4. If it's state-changing, write an audit row (`atlas_inventory::audit::record`).
5. Add/extend a test in `crates/atlas-gateway/tests/read_only.rs`.
6. Document it in `docs/API.md`.

## Adding a storage backend driver

1. New crate `crates/atlas-driver-<name>`; implement `atlas_driver_core::StorageDriver`.
2. Map the backend's native objects into `atlas-api-types` DTOs (add fields there if needed —
   that crate is the shared contract).
3. Register it in `crates/atlas-gateway/src/startup.rs` (`DriverRegistry`).
4. Keep read methods live; leave write methods as `NotImplemented` until the write slice.
5. Unit-test the parsing/normalization (see `atlas-driver-ceph`'s `real.rs` tests).

## Adding a database table / migration

- Add a new numbered file in `migrations/` (e.g. `0002_jobs.sql`), SQLite dialect.
  Never edit an already-shipped migration.
- SQLite mappings: timestamps → `TEXT` RFC3339 (`strftime('%Y-%m-%dT%H:%M:%fZ','now')`),
  JSON → `TEXT` + `CHECK(json_valid(col))`, auto-id → `INTEGER PRIMARY KEY AUTOINCREMENT`.
- Add upsert/read helpers in `atlas-inventory`.

## Tests

- Unit tests live beside the code (`#[cfg(test)] mod tests`).
- Gateway integration tests spin up the server with the **fake** driver + a throwaway SQLite
  file (`enable_k8s: false`, `initial_discovery: false`) — no external services required.

## Licensing

By contributing you agree to the [Contributor License Agreement](CLA.md) (so Zyvor
can license contributions under the Apache License, Version 2.0) and certify the [Developer Certificate of Origin](DCO.md) on every commit
(`git commit -s`).

Inbound Contributions are accepted under the Apache License 2.0 with the CLA
grant above. Don't submit code you don't have the rights to license that way. See
[`docs/LICENSING.md`](docs/LICENSING.md), [`NOTICE`](NOTICE), and
[`LICENSES/`](LICENSES/).

## Commits

- Keep commits focused; describe the *why*. Sign off with `git commit -s`
  (DCO). End with `Co-Authored-By: ...` where applicable.
- Don't commit `atlas.db*`, `target/`, or `.env` (see `.gitignore`).
