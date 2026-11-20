<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# atlas-driver-core

The pluggable-backend contract. Everything downstream depends on this, not on a concrete backend.

- **`StorageDriver`** (`#[async_trait]`) — the driver interface (PDF §17.2). Read methods
  (`discover`, `health`, `list_pools`, `list_volumes`, `metrics`) are live in slice 1; write
  methods default to `Err(DriverError::NotImplemented)` so slice 2 is additive.
- **`DriverError`** — `Backend` / `Unreachable` / `Parse` / `NotImplemented`.
- **`DriverRegistry`** — maps backend id → `Arc<dyn StorageDriver>`; `get`, `any`, `register`.

To add a backend: implement `StorageDriver` in a new `atlas-driver-*` crate and register it in
the gateway's `startup`.
