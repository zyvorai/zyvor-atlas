<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# atlas-driver-ceph

The Ceph `StorageDriver` implementation.

- **`RealCephDriver`** — shells out to `ceph`/`rbd` (**arg-arrays only**, `--format json`,
  per PDF §17.3) and normalizes `ceph status`, `ceph df detail`, `ceph osd tree`, `rbd ls -l`
  into Atlas DTOs. Selected by `ATLAS_CEPH_DRIVER_MODE=real`. Requires the ceph client + a
  reachable cluster (`/etc/ceph/ceph.conf` + keyring).
- **`FakeCephDriver`** — deterministic fixtures (3 pools, 2 RBD volumes, 6 OSDs) for local
  dev / tests / demo. Selected by `ATLAS_CEPH_DRIVER_MODE=fake`.
- **`ceph_cmd` / `rbd_cmd`** — the safe command wrappers.

Pool classification (`rbd`/`cephfs_data`/`cephfs_metadata`/`rgw`/`other`) is a name heuristic in
the MVP; a precise version would read `ceph osd pool application` metadata.

Verified end-to-end against a live Rook Ceph cluster (see [DEPLOYMENT.md](../../docs/DEPLOYMENT.md)).
