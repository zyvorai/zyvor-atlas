<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Atlas compared with WEKA, vSAN and others

WEKA (WekaFS) is a commercial parallel filesystem built for AI and HPC. Atlas is a storage control
plane: one API over several backends (Ceph, NFS, ZFS, Longhorn, S3) plus its own data plane,
atlas-native. This page says where each one is the better choice today. The WEKA column describes
WEKA's publicly documented feature set; it does not quote WEKA benchmark numbers. The Atlas
columns state only what has been measured or verified in this repository.

## Summary

- **Choose WEKA** when the job is the fastest possible shared filesystem for GPU training or HPC
  scratch on NVMe and RDMA, and a commercial licence is acceptable.
- **Choose Atlas** when you need one open control plane across mixed storage (including edge
  sites), database migration to the edge, cross-site DR on Ceph, and operations tooling, without a
  licence key.
- **They combine.** Atlas can register a WEKA cluster as one more backend (`atlas-driver-weka`,
  read-only discovery), so a WEKA estate shows up next to Ceph and ZFS in the same API and console.

## Matrix

| | **Atlas over Ceph** | **Atlas native** | **WEKA** |
|---|---|---|---|
| What it is | Control plane driving Ceph (RBD, CephFS, RGW) through Rook | Atlas's own replicated block and POSIX store | Parallel filesystem with its own client and data path |
| Data path | Ceph's (librbd, kernel RBD, CephFS) | FUSE client; writes through the Raft leader, reads direct from data nodes over a binary protocol (optional); data nodes on files or raw devices (`O_DIRECT` via `io_uring`), striped across several devices | Kernel-bypass NVMe and network stack |
| Measured performance | Ceph's; Atlas adds no data-path hop | Metadata engine ~9–12k creates/s on a 3-voter group; through FUSE ~270 creates/s, ~380 MiB/s write, up to ~540 MiB/s read, ~600 4 KiB random-read IOPS (one host, tmpfs, `docs/NATIVE_FS.md`); no NVMe numbers yet | Designed for very high IOPS and throughput at low latency |
| Metadata | Ceph MDS (CephFS) | Raft groups sharded by volume/filesystem; inodes and directory entries paged from an embedded KV store | Distributed across the cluster |
| Data protection | Ceph replication or erasure coding | 3 replicas or Reed-Solomon erasure coding (e.g. 4+2, 8+3), SHA-256 checksums per extent and per shard | Distributed erasure coding |
| Tiering to object storage | Not managed by Atlas | Cold extents to S3 (Ceph RGW by default, or any S3-compatible bucket), read back from their objects; snapshots export to S3 incrementally and import on any cluster | Yes |
| GPUDirect Storage | No | No | Yes |
| Protocols | Block, CephFS, S3 (RGW) | Block over HTTP, POSIX via FUSE, NFSv3/v4.1/v4.2 (NFS-Ganesha gateway), SMB 2.1/3.x (Samba gateway), S3 over the same files | POSIX, NFS, SMB, S3 |
| Snapshots and clones | Yes (RBD, CephFS) | Yes, copy-on-write | Yes |
| Cross-site DR | Two-way RBD mirroring with clean failback, verified live (`docs/DR.md`) | Asynchronous filesystem replication: snapshot increments that send only changed extents, read-only replica, promote and demote for failover and failback; verified live between two clusters on one lab host (`docs/NATIVE_REPLICATION.md`) | Snapshot-to-object and replication features |
| Mixed backends under one API | Ceph, NFS, ZFS, Longhorn, S3, macOS, native | (same gateway) | WEKA only |
| Cloud-to-edge DB migration | DataBridge: six engines, CDC, cutover | (same gateway) | Not in scope |
| Operations | AI Ops Advisor, anomaly detection, eBPF I/O RCA (`atlas-io`); MCP server with 31 read-only tools and two opt-in, audited, two-phase writes (`create_snapshot`, `ack_alert`) (`docs/MCP.md`) | (same gateway) | Own management GUI and monitoring |
| Edge footprint | Single-node Rook lab profile | Single-node low-memory profile (`values-edge.yaml`): one pod, catalog memory capped at ~26 MB however many files, 61 MB peak in a 50k-file lab run (`docs/NATIVE_EDGE.md`) | Multi-node clusters on qualified hardware |
| Licence | Apache-2.0, no key | Apache-2.0, no key | Commercial |

## WEKA as an Atlas backend

`atlas-driver-weka` registers a WEKA cluster as backend `bkd_weka`. It is read-only: WEKA
filesystems appear as `filesystem` volumes (native id `weka-fs:<uid>`) in one `weka` pool, with
cluster capacity, health and `weka_*` metrics. Atlas never creates or changes anything on WEKA.

| Variable | Meaning |
|---|---|
| `ATLAS_WEKA_ENABLE=1` | Register the backend |
| `ATLAS_WEKA_DRIVER_MODE` | `fake` (default, two fixture filesystems) or `real` |
| `ATLAS_WEKA_ENDPOINT` | API base, e.g. `https://weka01:14000/api/v2` |
| `ATLAS_WEKA_USERNAME`, `ATLAS_WEKA_PASSWORD_FILE`, `ATLAS_WEKA_ORG` | Login (`POST /login`); a rejected token triggers one re-login |
| `ATLAS_WEKA_CA_CERT`, `ATLAS_WEKA_TIMEOUT_SECS` | TLS trust and request timeout (10 s) |

Real mode reads `GET /cluster` (`capacity.total_bytes`, `capacity.unprovisioned_bytes`, `status`)
and `GET /fileSystems` (`name`, `uid`, `status`, `total_budget`, `used_total`). It is tested
against a mock of those endpoints, not yet against a live WEKA cluster; any field the cluster
doesn't return is reported as unknown.

## Closing the data-plane gap

The atlas-native roadmap, in order, each phase gated on a published benchmark:

1. **Data path:** clients read and write data nodes directly (striped across replicas) over a
   binary streaming protocol; an `io_uring` device backend with `O_DIRECT` on raw NVMe. Parallel
   extent I/O, group commit, direct client reads, the `io_uring`/`O_DIRECT` backend and
   multi-device striping are done; direct client writes and an NVMe benchmark are not.
2. **Metadata scale:** namespace sharded across Raft groups, an on-disk catalog, client leases.
   Commits cost one fsync, Raft replication is pipelined (a 3-voter group sustains ~12–15k
   creates/s on one host), and checkpoints write only changed records to an embedded KV store.
   Inode tables and directory entries are paged from the store, volumes and filesystems shard
   across Raft groups, client sessions hold cross-mount file locks, and cache leases let mounts
   cache metadata without serving it stale. Filesystem snapshot trees are still held in memory.
3. **Efficiency:** erasure coding with a rebuild controller; cold extents tiered to S3.
   Reed-Solomon k+m extents (4+2 stores 1.5× the data instead of 3×) and a prioritised, paced
   rebuild controller with incremental scrub, tiering of cold extents to S3, and snapshot
   export to (and import from) S3 are done.
4. **AI:** RDMA transport, a GPUDirect Storage path, a checkpoint fast path, a CSI driver;
   MLPerf Storage results. The checkpoint fast path (unserialized whole-extent writes,
   background parallel write-back), the CSI driver (`docs/NATIVE_CSI.md`) and a dataset
   locality API (query and pin a dataset's placement per host) are done; RDMA, GPUDirect Storage
   and MLPerf Storage results are not.
5. **Protocols:** NFS, SMB and S3 front ends over the same namespace; ACLs, quotas, `O_DIRECT`;
   an IO500 submission. Per-filesystem byte and inode quotas, POSIX ACLs, `O_DIRECT`, an NFS gateway
   (`docs/NATIVE_NFS.md`), an S3 gateway (`docs/NATIVE_S3.md`) and an SMB gateway
   (`docs/NATIVE_SMB.md`) are done. An IO500 harness runs the full suite (`docs/IO500.md`), but
   there is no submission: that waits for NVMe benchmark hardware.
6. **Edge and DR:** asynchronous filesystem replication with failover and failback, a
   single-node low-memory edge profile, two-way RBD mirroring on the Ceph side. All three are
   done: filesystem replication (`docs/NATIVE_REPLICATION.md`), the edge profile
   (`docs/NATIVE_EDGE.md`), and two-way RBD mirroring with a guarded clean failback, verified
   live between two Rook clusters (`docs/DR.md`).

Status of each step is tracked in `docs/NATIVE_STORAGE.md`. Until a phase's benchmark is
published, treat atlas-native as behind WEKA on that axis.

## Compared with NetApp ONTAP and Nutanix

Vendor columns are from their public documentation (ONTAP 9.18.1 and AFX release notes, Nutanix
Unified Storage and Data Lens pages), not tested. They are where each product is **ahead of**
Atlas today as well as where Atlas differs.

| | **Atlas** | **NetApp ONTAP** | **Nutanix NUS** |
|---|---|---|---|
| Tamper-proof snapshots | Retention-locked block and filesystem snapshots on atlas-native, Raft-enforced, extend-only (`docs/WORM.md`); not on Ceph/ZFS | SnapLock, tamper-proof snapshots | Object WORM, immutable snapshots |
| Ransomware detection | Explainable entropy + rename/delete/burst scorer over native filesystem activity; opt-in auto snapshot-and-lock. Lab-verified 2026-10-26 on a synthetic encrypt-and-delete burst (verdict, auto-snapshot, lock refusal); not trained, no S3 signal yet, not tried against real ransomware | Autonomous Ransomware Protection with AI, on by default for new volumes | Data Lens anomaly and user-behaviour detection |
| S3 object retention | Compliance mode on the native S3 gateway (lab-verified 2026-10-26: locked objects refuse delete and overwrite, retention cannot be shortened) | S3 Object Lock | Objects WORM |
| Data reduction | Opt-in LZ4 compression and extent dedup on atlas-native, with a per-volume savings report (`docs/NATIVE_DATA_REDUCTION.md`); unit/integration tested; byte-exact read-back and savings report lab-verified 2026-10-26 on a multi-process cluster (favourable input); single-core LZ4 and SHA-256 costs measured, no end-to-end throughput; Ceph/ZFS do their own | Inline dedup, compression, compaction | Dedup, compression, erasure coding |
| Replication / DR | Snapshot replication, RBD mirroring with failback, verified on two lab clusters | SnapMirror, MetroCluster | Async, NearSync, Sync, Metro |
| Protocols | Block, POSIX, NFS, SMB, S3 | NFS, SMB, S3, FC, iSCSI, NVMe-oF | Files, Objects, Volumes (iSCSI) |
| Multi-vendor control plane | Yes: Ceph, ZFS, NFS, Longhorn, WEKA, macOS, native, any S3 | ONTAP (and cloud variants) | Nutanix clusters |
| Open source, no licence key | Apache-2.0 | No | No |
| Performance proof | Lab numbers only, no NVMe benchmark | Published vendor benchmarks | Published vendor benchmarks |

## Compared with VMware vSAN (hyperconverged infrastructure)

vSAN is the storage half of VMware's hyperconverged stack: it pools the local disks of vSphere hosts
and runs inside the hypervisor. Atlas is only a storage layer and has no hypervisor, so the
comparison is storage against storage; the compute side is Zeus OS and KubeVirt, Kairon or Machina.
The vSAN column is general product knowledge of the public feature set, not tested here, and was not
checked against a current VMware page; verify it before quoting it.

| | **Atlas** | **VMware vSAN** |
|---|---|---|
| Where it runs | Beside any hypervisor or Kubernetes: Ceph, NFS, ZFS, Longhorn, macOS, WEKA (read-only), atlas-native, any S3 | Inside the vSphere hypervisor, on vSAN-ready hosts |
| Compute coupling | None; storage is separate from the VM platform | Tied to vSphere |
| Pooled local disks | atlas-native across your nodes; or Ceph through Rook | Yes, its core job |
| Data protection | 3 replicas or Reed-Solomon (4+2, 8+3), SHA-256 checksums per extent and shard | Mirroring and erasure coding by storage policy |
| Data reduction | Opt-in LZ4 and extent dedup on atlas-native, unit tested only (`NATIVE_DATA_REDUCTION.md`) | Dedup and compression |
| Snapshots and clones | Copy-on-write, block and filesystem | Yes |
| Multi-site | Async replication and two-way RBD mirroring, guarded failback; no synchronous stretched cluster | Stretched clusters, synchronous across two sites |
| Remote and edge sites | Single-node profile, ~26 MB catalog memory | Small clusters on qualified hardware |
| Protocols | Block, POSIX, NFS, SMB, S3, Kubernetes CSI | vSphere datastores; file services as an add-on |
| Hardware support | Any Linux host; lab-verified only, no NVMe benchmark | Hardware compatibility list and vendor support |
| Licence | Apache-2.0, no key | Commercial subscription |

**Choose vSAN** when the estate stays on vSphere and you want one vendor to support compute and
storage together, or you need a synchronous stretched cluster. **Choose Atlas** when you are leaving
the hypervisor, want storage that does not depend on it, or run mixed backends and edge sites.

## Leaving VMware: moving the VMs to Atlas-backed storage

Atlas does not read VMware disks. The migration is done by the rest of the Zyvor suite, and Atlas
holds the result.

| Step | Tool | Notes |
|---|---|---|
| Discover and score | [Scout](https://github.com/zyvorai/scout) | RVTools import, readiness and executive report |
| Export | [Transiva](https://github.com/zyvorai/zyvor-transiva) | Exports VMs, including Nutanix AHV, to disk files |
| Pick up, repair offline, convert | [h2kvm](https://github.com/zyvorai/zyvor-h2kvm) with GuestKit | From vSphere, ESXi over SSH, Azure, or VMDK, VHD(X), OVA/OVF and raw files; fixes fstab, bootloader, initramfs and drivers before first boot; writes qcow2 and runs `qemu-img check` |
| Land and run | Kairon with Veyron, Machina, or KubeVirt (legacy target) | h2kvm deploys with `--deploy-kairon` or `--emit-domain-xml`; `--deploy-k8s` for KubeVirt |
| Store | Atlas | Ceph RBD, CephFS, NFS, ZFS or atlas-native behind a StorageClass |

Limits to state plainly:

- **Direct import is raw-only and atlas-native-only.** `POST /volumes/{id}/import` streams a raw image
  into an atlas-native volume, resumes from recorded progress and verifies the volume by reading it back
  (`IMAGE_IMPORT.md`). h2kvm writes qcow2, which that job cannot read, so convert to raw first. Ceph RBD
  targets still go through CDI, with no Atlas-side resume offset or recorded checksum. The import has not
  been run on a real cluster or a large image.
- **Licence differs.** h2kvm is source-available under the Zyvor Production License: free for
  evaluation and labs, a paid subscription for production. Atlas stays Apache-2.0.
- **Source coverage is partial.** AWS and Proxmox Backup Server are library modules only, and GCP,
  Xen, VirtualBox and remote Hyper-V have no pickup code (their disk files work as local files).
- **Naming.** Some Atlas docs call Transiva the converter and use its old owner id `hyper2kvm`. In
  the h2kvm repository, h2kvm converts and Transiva exports. See `PRODUCTS.md`.

## Other storage MCP servers

Atlas is not the only storage system an AI agent can talk to. As of this writing MinIO ships an
[MCP server for AIStor](https://www.min.io/blog/mcp-server-for-aistor) (object storage), and Google Cloud's
[NetApp Volumes](https://docs.cloud.google.com/netapp/volumes/docs/deploy-use-cases/mcp/use-netapp-mcp) has a
remote MCP server for pools and volumes; community servers exist for other arrays. Those cover one
product or service. Atlas's MCP server sits on a control plane that already spans Ceph, NFS, ZFS, Longhorn
and its own native backend, defaults to read-only, and keeps roles, tenant scoping and audit identical to
the REST API. Check each vendor's own documentation for what theirs exposes today.

WEKA and WekaFS are trademarks of WekaIO, Inc. Ceph, Rook and Longhorn are trademarks of their
respective owners.
