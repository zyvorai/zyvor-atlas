<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# atlas-api-types

The **shared contract**: serde DTOs used by the gateway, drivers, and inventory. Pure data, no
logic, no I/O — so it can be depended on everywhere without coupling.

Backend-agnostic by design (PDF §3.3): a `StorageVolume` may be backed by Ceph RBD today and
NFS/ZFS/SAN/cloud tomorrow.

Key types:
- Enums: `BackendType`, `BackendMode`, `Health`, `VolumeKind`.
- Resources: `StorageBackend`, `Capabilities`, `StorageCluster`, `StoragePool`, `StorageVolume`,
  `Osd`, `StorageHealth`, `MetricSample`, `StorageClassInfo`.
- `DiscoveryResult` — a full driver discovery pass (cluster + pools + osds + volumes + health).
- Write-path request/result types (`CreateVolumeRequest`, `CreateSnapshotRequest`, …) — defined
  now for a stable API surface; used once the write path lands (slice 2).
