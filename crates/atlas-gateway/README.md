<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# atlas-gateway

The REST edge of the control plane (binary: `atlas-gateway`). axum 0.8.

Modules:
- **`startup`** — `build_state(config, opts)`: open DB + migrate, register the Ceph backend +
  driver (real/fake), attach the live k8s driver if reachable, optional initial discovery.
  Shared by the binary and integration tests.
- **`state`** — `AppState { pool, config, drivers, k8s }`; `driver_for(id)`.
- **`auth`** — JWT middleware (`ATLAS_AUTH_REQUIRED`); injects an `Actor` extension.
- **`routes`** — the read-only v1 surface (see [../../docs/API.md](../../docs/API.md)).

Run: `make run` (fake driver) or set `ATLAS_CEPH_DRIVER_MODE=real` with a reachable cluster.
Tests: `crates/atlas-gateway/tests/read_only.rs` (fake driver, throwaway SQLite, no cluster).
