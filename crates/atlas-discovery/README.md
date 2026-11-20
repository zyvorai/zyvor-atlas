<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# atlas-discovery

The discovery worker (PDF §8.1). Thin glue between a driver and the inventory.

`run_discovery(pool, driver)`:
1. calls `driver.discover()` (ceph/rbd CLI or fixtures),
2. persists via `atlas_inventory::upsert_discovery`,
3. logs a `storage.backend.discovered` event and returns a `DiscoverySummary`
   (`backend_id`, `cluster_id`, counts of pools/osds/volumes).

Invoked by the gateway at startup (optional) and on `POST /backends/{id}/discover`.
