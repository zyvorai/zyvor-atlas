<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# atlas-native compared with Ceph: gap analysis

**Verdict: atlas-native cannot replace Ceph today.** It is a replicated store with real
filesystem, S3, NFS, SMB and CSI front ends, but data does not spread across added nodes, a default
three-node cluster stops writing when one node is down, there is no block device path, and no
cluster spanning more than one physical host is recorded as verified. [STATUS.md](STATUS.md) marks it experimental and
not production-supported.

## How to read this

- **Basis:** source read at commit `9c2aba1e`. Nothing was built or run for this page. *Inferred*
  marks behaviour concluded from code that no test or lab record was found to demonstrate; reproduce
  it before relying on it.
- Code paths without a prefix are under `crates/atlas-native/src/`.
- Ceph statements come from the linked official pages (`latest` docs, which may describe a newer
  release than the one you run). Ceph was not tested for this page.
- **Severity** for replacing Ceph: **blocker** (cannot deploy for that workload), **major** (deployable
  with serious risk or operational cost), **minor**.
- **Size** is an unvalidated engineering guess for one experienced developer: **S** up to 2 weeks,
  **M** 1-2 months, **L** 3-6 months, **XL** more.
- No performance comparison is made. None has been measured.

## Changed since the basis commit

The tables below describe `9c2aba1e`. Later work on the same branch:

- **A1 (block device):** `atlas-native-ublk` exists ([NATIVE_UBLK](NATIVE_UBLK.md)), experimental: its functional checks passed on one lab host (kernel 7.0, cluster on localhost, 2026-11-28). No multi-host, memory-pressure or performance verification, and no CSI block volumes.
- **A2 (small writes):** opt-in delta writes exist ([NATIVE_DELTA_WRITES](NATIVE_DELTA_WRITES.md)), measured there: faster than Ceph in one 60 s test; over 20 minutes (three rounds, with parallel folding) 1 MiB cells were slower, 128 KiB cells level on write rate while falling behind on folding, and p99 latency was worse in both.
- **A3 (stale block reads):** block data reads now run a read barrier, and the read lease is granted only from a confirmed ReadIndex. Tested in process, not on a real multi-host partition.
- **A6 (S3 features), in part:** object tagging, and conditional writes (`If-None-Match: *`, `If-Match` on put, copy and multipart completion) that are atomic across gateway replicas ([NATIVE_S3](NATIVE_S3.md#conditional-writes)). Tested on one host. Versioning, lifecycle, policies, ACLs, bucket tagging, conditional deletes, SSE, notifications and multisite are still missing.

## Top blockers

1. New data keeps landing on the same nodes (*inferred* from code); no live add, drain or rebalance
   (P1-P3).
2. With replicas equal to nodes (the default 3 and 3), one node down stops writes, rolling restarts
   included (F1).
3. Block volumes are reachable only over HTTP, small writes rewrite whole extents one at a time, and
   a read can return stale data (A1-A3).
4. Extent metadata is held in memory on every metadata node and sharding does not move it off any
   of them (S1, S2); a lost catalog cannot be rebuilt from the data devices (F5).
5. One shared API token (O4), and no multi-host, power-loss or disk-failure verification.

## Block, file and object access

| # | Gap | Ceph | atlas-native today | Sev | Size |
|---|---|---|---|---|---|
| A1 | No block device | Kernel module and QEMU/KVM [(RBD)][rbd]; QEMU attaches through `librbd` [(QEMU)][qemu]; Ceph-CSI maps RBD images on nodes [(k8s)][rbdk8s] | HTTP `PUT`/`GET /v1/volumes/{id}/data` (`node.rs:1355-1372`); the gateway proxies at most 4 MiB per request ([NATIVE_NODE](NATIVE_NODE.md)). No NBD, ublk, iSCSI, NVMe-oF or QEMU code in the native crates or chart. CSI refuses block (`crates/atlas-native-csi/src/controller.rs:151-155`) | blocker (block, VMs) | L |
| A2 | Small and concurrent writes | Objects accept writes at a byte offset [(librados)][librados] | A partial-extent write reads, merges and rewrites the whole extent (4 MiB default for volumes, so a 4 KiB overwrite of a full uncompressed extent stores 12 MiB at 3 replicas), under one engine-wide lock per metadata group (`engine.rs:569-576`, `590-615`). Aligned writes place data outside the lock but still commit one at a time under it (`engine.rs:692-699`) | blocker (block); major (random-write files) | L |
| A3 | Stale block reads | RBD relies on RADOS strong consistency [(RBD)][rbd] | Volume data `GET` reads the receiving replica's catalog with no leader check or read barrier (`node.rs:1364-1372`, `engine.rs:1094`); the filesystem API gates reads (`node/fs_api.rs:32-41`). The driver sends reads to whichever endpoint last answered (`crates/atlas-driver-native/src/http.rs:156-185`). [NATIVE_NODE](NATIVE_NODE.md) says any replica serves reads. A lagging follower can miss an acknowledged write (*inferred*) | blocker (block) | S |
| A4 | File clients | Linux kernel CephFS driver [(mount)][cephfsk] | FUSE only, Linux only. No request pipelining or batching; a read verifies whole extents ([NATIVE_FS](NATIVE_FS.md), "Known limits") | major | L |
| A5 | NFS and SMB availability | NFS module offers a virtual IP with failover [(nfs)][nfs]; SMB module offers clustering and AD membership [(smb)][smb] | One pod each. NFS: `sec=sys` only, no grace-period recovery. SMB: local users only, no Windows ACLs, no durable handles ([NFS](NATIVE_NFS.md), [SMB](NATIVE_SMB.md)) | major | L |
| A6 | S3 features | Versioning, lifecycle, bucket policies, ACLs, tagging, notifications [(S3)][s3]; multisite, including active-active [(multisite)][multisite] | Bucket create/delete, versioning, lifecycle, policies, ACLs, tagging, SSE and notifications return `NotImplemented`; conditional `PUT` is not atomic ([NATIVE_S3](NATIVE_S3.md)) | blocker if those are used, else major | L |

## Scale, placement and node lifecycle

| # | Gap | Ceph | atlas-native today | Sev | Size |
|---|---|---|---|---|---|
| P1 | Data does not spread | CRUSH weights reflect device size [(CRUSH)][crush]; a balancer is on by default [(balancer)][balancer] | `free_bytes` is a static config value (default 1 TiB, `node.rs:346`, `708`) that nothing updates. Placement sorts by it, then by node id (`placement.rs:38-51`, `engine.rs:1750-1761`). With equal values every new extent goes to the same `replicas` (or k+m) nodes; others receive data when one of those fails or backs off, or through a manual filesystem locality pin. Scaling 3 to 5 pods therefore adds spares, not usable capacity (*inferred*; no distribution test found) | blocker | M |
| P2 | Add, drain, remove, rebalance | Adding an OSD migrates data to it; marking one `out` drains it while it runs [(add/remove)][addrm] | Data nodes are static config (`node.rs:278-296`); a change restarts every metadata pod (ConfigMap checksum in the chart). No drain, weight or rebalance. A node removed from config is not in the rebuild controller's lost set (`engine/rebuild.rs:27-42`), so only a paced scrub or a manual `POST /v1/repair` restores its copies, and degraded gauges do not count them (*inferred*) | blocker | M |
| P3 | Capacity and full handling | `nearfull` 0.85 and `full` 0.95 ratios; full pools block writes [(capacity)][capacity] | No device capacity in placement and no full thresholds. A full device shows up as a failed write (`raw.rs:243-247` for block devices) and a 5 s back-off, which a later successful probe clears. The gateway reports no capacity (`crates/atlas-driver-native/src/lib.rs:390-392`) | blocker | M |
| P4 | Failure domains | Hierarchy from OSD through host, rack and datacenter, plus device classes [(CRUSH)][crush] | Rack and host only; `zone` is never read by placement (`placement.rs`). The Helm chart sets neither (`deploy/helm/atlas-native/templates/_helpers.tpl:64`), so each pod counts as its own host (`node.rs:705-706`) while pod anti-affinity is `soft`: copies can share a physical machine | major | S |
| P5 | Disks | One OSD per device, replaceable keeping its ID [(add/remove)][addrm] | The failure and placement unit is the node. Device order and count are fixed config ([NATIVE_NODE](NATIVE_NODE.md), "Devices"). No disk replacement procedure; free-list ranges of a replaced file device can point past its new end, where writes fail (`device.rs:122-128`, *inferred*) | major | M |
| S1 | Metadata in memory | CephFS keeps metadata in a RADOS pool served by MDS daemons [(CephFS)][cephfs] | Inodes and directory entries are paged; volumes, snapshots, **all extents** and filesystem snapshot trees load into memory (`store.rs:227-230`), and the leader clones the catalog to validate proposals (`raft.rs:948-955`). 1 PiB on a 4 MiB grid is 268 million extent records per replica. Bytes per extent record are **not measured** | blocker at scale | L |
| S2 | Raft groups do not scale out | Multiple active MDS daemons split subtrees [(multi-MDS)][mds] | Up to 64 groups, all on the **same voters** ([NATIVE_NODE](NATIVE_NODE.md), "Metadata groups"), so each metadata node holds every group's catalog. Count can only be raised; existing objects never move. One filesystem stays in one group behind one leader. Space freed in one group is reusable only by that group | major | XL |
| S3 | Whole-catalog scans | Not compared | GC scans every extent each interval (`gc.rs:14-21`); during a rebuild every 64-extent batch rescans all extents (`rebuild.rs:152`, `engine/rebuild.rs:62-97`) | major at scale | S |
| S4 | Space reclaim | Not compared | Appends that never commit are not reclaimed ([NATIVE_FS](NATIVE_FS.md)); no hole punching, TRIM, volume discard or compaction; first-fit free list kept whole in memory (`alloc.rs`) | major | M |

## Failure handling and recovery

| # | Gap | Ceph | atlas-native today | Sev | Size |
|---|---|---|---|---|---|
| F1 | No degraded writes | A placement group can take I/O with fewer than `size` copies, down to `min_size` [(pools)][pools] | A write needs all `replicas` (or all k+m) targets, else `InsufficientReplicas` (`engine.rs:931-970`). With no spare node, one failure or restart stops writes. A fourth data node serves as that spare | blocker (3-node) | S to require and check a spare; M for degraded writes |
| F2 | Disk failure detection | A disk is replaced as its own OSD [(add/remove)][addrm] | Any successful I/O clears a node's down timer (`engine.rs:1739-1746`) and the probe reads only device 0 (`engine/rebuild.rs:47-59`). One dead disk among several may never mark the node lost, leaving repair to scrub (*inferred*) | major | S |
| F3 | Recovery parallelism and impact | Recovery runs on OSD hosts [(arch)][arch]; mClock divides OSD IOPS among client, recovery and scrub [(mClock)][mclock] | One thread per metadata node steps the groups it leads in turn (`node.rs:944-948`), each repairing one extent at a time through the leader (`rebuild.rs:166-176`) and holding the group write lock while writing each replacement (`engine.rs:1353`). Rate caps exist (rebuild 256 MiB/s, scrub 64 MiB/s per group) | major | M |
| F4 | Scrub at size | Light scrub daily, deep scrub weekly by default, per placement group [(scrub)][scrub] | One sequential pass per group; the cursor is in memory and restarts with each leader term (`rebuild.rs`, `node.rs:962-964`). At the 64 MiB/s cap, reading 30 TiB of copies takes about 5.7 days (arithmetic, one group) | major | S |
| F5 | Metadata disaster recovery | Monitor store can be rebuilt from OSDs [(mon recovery)][monrec] | Devices hold raw payloads with no self-describing headers (`device.rs`), so a lost catalog is not recoverable from them ([design note](NATIVE_SNAPSHOT_TREE_PAGING.md)). No force-new-quorum or catalog rebuild tool found. Off-cluster copies: block snapshot export to S3 and filesystem replication | blocker | M for backup and restore; L for rebuild |
| F6 | Disaster recovery coverage | RBD mirroring, journal or snapshot based [(mirroring)][mirror] | Asynchronous replication for filesystems only ([NATIVE_REPLICATION](NATIVE_REPLICATION.md)); block volumes have manual snapshot export/import | major | M |

## Consistency and integrity

What holds, from code and tests: copy-on-write extents, SHA-256 verified on read with fallback to
another copy, one Raft command per write request's extents, term fencing at data nodes, and a
simulated-network Raft fault test (`crates/atlas-native/tests/raft.rs:335`, three seeds, metadata
only).

| # | Gap | Ceph | atlas-native today | Sev | Size |
|---|---|---|---|---|---|
| C1 | Stale block reads | See A3 | See A3 | blocker | S |
| C2 | Leader filesystem reads | Not compared | Default `GET` checks the local leader role and applied index, with no quorum round (`engine.rs:1423-1428`, `raft_server.rs:619-649`). A partitioned old leader could briefly serve stale data until check-quorum demotes it (*inferred*; `?barrier=1` avoids it) | minor | S |
| C3 | Metadata checksums | BlueStore checksums all metadata and data, `crc32c` by default [(BlueStore)][bluestore] | WAL records are JSON lines with no per-record checksum; replay checks only that they parse and that indexes rise (`wal.rs:27-31`, `190-229`). No cross-replica catalog comparison found | major | S |
| C4 | Client-side direct writes | Not compared | With `--direct-writes` the committed checksum is the client's word until scrub ([NATIVE_FS](NATIVE_FS.md)); off by default | minor | S |
| C5 | Cross-mount semantics | Not compared | Close-to-open for file data; unlink-while-open protects only the same mount ([NATIVE_FS](NATIVE_FS.md), "Consistency") | major for shared-write apps | L |

## Operations and security

| # | Gap | Ceph | atlas-native today | Sev | Size |
|---|---|---|---|---|---|
| O1 | Upgrades | `cephadm` upgrades in a fixed order, restarting a daemon only when the cluster will stay available, with pause and resume [(upgrade)][upgrade] | Manual ordering rules per feature ("upgrade every metadata node first"). No protocol or catalog version negotiation found; no downgrade past the catalog store change. Readiness checks only for a metadata leader (`node.rs:1613-1628`), so a rolling update does not wait for data redundancy | major | M |
| O2 | Monitoring and health | Manager Prometheus module with cluster metrics and health checks [(prometheus)][prom] | Prometheus metrics, `/v1/status` and a ServiceMonitor. No native alert rules or dashboard found in `deploy/observability/`, no capacity (P3) | major | S |
| O3 | Quotas | Pool byte and object quotas [(pools)][pools] | Per-filesystem byte and inode quotas. No cluster, pool or tenant limit enforced by the cluster; Atlas gateway quotas are admission checks | major | S |
| O4 | API identity | `cephx` per-entity keys with capabilities [(auth)][cephx] | One optional bearer token for all `/v1/*` (`node.rs:1238-1241`); the chart's gateways and CSI use it. A data node accepts any client certificate from the cluster CA; no revocation ([NATIVE_METADATA](NATIVE_METADATA.md)). Plaintext unless TLS is configured. Without `grant_key_file`, any client a data node accepts can write to it | blocker (multi-tenant) | M |
| O5 | Retention bypass | Not compared | Any holder of the node token can clear a snapshot lock (`node.rs:1408`, [WORM](WORM.md)); the two-person rule is only in the gateway | major | S |
| O6 | Encryption at rest | `dmcrypt` OSDs [(encryption)][dmcrypt] | None in the native crates; use encrypted volumes underneath (not tested) | major if required | S to document and test host encryption |
| O7 | Dedup side channel | Not compared | Opt-in dedup shares across tenants in a group ([NATIVE_DATA_REDUCTION](NATIVE_DATA_REDUCTION.md)) | minor (off by default) | M |

## Ecosystem

| # | Gap | Ceph | atlas-native today | Sev | Size |
|---|---|---|---|---|---|
| E1 | Kubernetes | Ceph-CSI RBD block volumes [(k8s)][rbdk8s] | Filesystem volumes only; no topology; restarting a node plugin pod breaks its mounts; multi-node RWX unverified ([NATIVE_CSI](NATIVE_CSI.md)) | blocker for block PVCs; major otherwise | M (restarts, multi-node); block is A1 |
| E2 | VMs and clouds | QEMU, libvirt and OpenStack use RBD [(RBD)][rbd] | No driver for any of them. A disk image file on a mount is untested | blocker (VM estates) | L-XL after A1-A3 |

## Verification maturity

Ceph's integration framework provisions multiple machines and includes thrash and power-cycle suites
[(teuthology)][teuth]. For atlas-native, per [STATUS.md](STATUS.md) and the `NATIVE_*` pages:

| Area | Evidence found | Missing |
|---|---|---|
| Cluster, failover | In-process tests; 3-pod StatefulSet and leader-pod failover on a **single-node** k3s | Separate hosts, real partitions |
| Rebuild, scrub, EC | Tests (`tests/rebuild.rs`, `health.rs`, `erasure.rs`) | No lab record of node or disk loss with rebuild |
| POSIX | pjdfstest (8565 tests), git clone and fsck, 3 nodes on one host, 2026-10-04 | No xfstests record; no rerun record for later changes |
| Durability | Torn-WAL and restart tests, simulated Raft faults | No power-loss test; no linearizability checker over the running system |
| Front ends | NFS, SMB, S3, CSI, replication on one lab host | Multi-node, HA |
| Devices, scale | One data-path run on a shared HDD, three nodes in one process ([NATIVE_PERF](NATIVE_PERF.md)) | NVMe, `io_uring` results, multi-host scaling, large data sets, soak |
| RDMA, GPU, direct writes | SoftRoCE plumbing; GPU fixtures only; direct writes in-process only | Any hardware or lab run |

Closing this is a **blocker**, size **M** once hardware exists (at least four hosts with spare disks).

## Where atlas-native is simpler, or already ahead

Narrow points, each with its limit.

- **Fewer daemon types.** One binary runs the data and metadata roles (`bin/atlas-native-node.rs`,
  [NATIVE_NODE](NATIVE_NODE.md)); Ceph sizes OSD, monitor, manager and MDS daemons separately
  [(hardware)][hw]. Front ends are still separate processes, and part of the simplicity is the missing
  features above.
- **Filesystem clones copy no file data.** A clone shares extents with its snapshot
  ([NATIVE_FS](NATIVE_FS.md); lab-checked for isolation). CephFS subvolume clone is an asynchronous
  bulk copy [(fs volumes)][fsvol]. The cost here is metadata proportional to inode count, held in
  memory (S1).
- **S3 and POSIX see the same files.** An S3 object is the file NFS, SMB and FUSE see
  ([NATIVE_S3](NATIVE_S3.md); aws-cli on the lab). The RGW page checked describes a shared namespace
  between S3 and Swift only [(RGW)][rgw]. S3 keys bypass POSIX permissions.
- **Small single-node footprint.** The edge profile peaked at 61 MB resident in a 50,000-file lab run
  ([NATIVE_EDGE](NATIVE_EDGE.md)). `osd_memory_target` defaults to 4 GiB per OSD [(hardware)][hw].
  These are a measurement against a default setting, not a like-for-like test, and the edge profile
  keeps one copy.
- **End-to-end content check.** Each extent's SHA-256 lives in metadata, apart from the data, and
  mounts using the optional direct reads verify it themselves. BlueStore's documented default is
  `crc32c` [(BlueStore)][bluestore]. The cost is that small reads fetch and hash whole extents (A4).

## Ranked roadmap

Ordered by risk removed per unit of work. It aims first at a safe small **file and object** cluster;
block comes later because it needs several of these first.

| Rank | Item | Closes | Size | Done when |
|---|---|---|---|---|
| 1 | Gate block reads as filesystem reads are; reproduce A3 first with a lagging follower | A3, C1 | S | A test that fails today passes |
| 2 | Capacity-aware, spread placement: data nodes report free space, deterministic hot spots removed, near-full and full limits, real failure domains from Kubernetes | P1, P3, P4 | M | New writes spread over N > replicas nodes; filling a node degrades predictably |
| 3 | Runtime data-node lifecycle: add, drain, remove and rebalance without restarting metadata pods; per-disk health; removed nodes counted as degraded; repair in parallel without the write lock; health-gated rolling restart with a required spare | P2, P5, F1-F4, O1 | L | Replace a node and a disk under load with no write outage |
| 4 | Metadata backup and restore, WAL record checksums, scoped credentials (read, write, admin; unlock separated) | F5, C3, O4, O5 | M | Restore drill from total metadata loss; a gateway token cannot unlock |
| 5 | Multi-host verification: four or more hosts, power cuts, partitions, disk pulls, xfstests, a history checker, soak | Verification | M | Results recorded in STATUS.md |
| 6 | Measure bytes per extent, then page extent maps (or put groups on separate voters) | S1-S3 | L | Memory bounded as data grows, measured |

Only then start block access (A1, A2, E1, E2): sub-extent writes with per-volume concurrency, a
client-side data path with `ublk` or NBD, CSI block volumes, then VM integration. Until then keep
Ceph for block, VMs and any cluster that must grow.

## Not covered

Cost, licensing, hardware sizing, RDMA and GPU paths, Ceph features not listed above (stretch
clusters, for example), and any performance comparison. Update this page when a row
is closed; check the code, not only the feature docs.

[arch]: https://docs.ceph.com/en/latest/architecture/
[addrm]: https://docs.ceph.com/en/latest/rados/operations/add-or-rm-osds/
[balancer]: https://docs.ceph.com/en/latest/rados/operations/balancer/
[bluestore]: https://docs.ceph.com/en/latest/rados/configuration/bluestore-config-ref/
[capacity]: https://docs.ceph.com/en/latest/rados/configuration/mon-config-ref/
[cephfs]: https://docs.ceph.com/en/latest/cephfs/
[cephfsk]: https://docs.ceph.com/en/latest/cephfs/mount-using-kernel-driver/
[cephx]: https://docs.ceph.com/en/latest/rados/configuration/auth-config-ref/
[crush]: https://docs.ceph.com/en/latest/rados/operations/crush-map/
[dmcrypt]: https://docs.ceph.com/en/latest/ceph-volume/lvm/encryption/
[fsvol]: https://docs.ceph.com/en/latest/cephfs/fs-volumes/
[hw]: https://docs.ceph.com/en/latest/start/hardware-cpu-memory/
[librados]: https://docs.ceph.com/en/latest/rados/api/librados/
[mclock]: https://docs.ceph.com/en/latest/rados/configuration/mclock-config-ref/
[mds]: https://docs.ceph.com/en/latest/cephfs/multimds/
[mirror]: https://docs.ceph.com/en/latest/rbd/rbd-mirroring/
[monrec]: https://docs.ceph.com/en/latest/rados/troubleshooting/troubleshooting-mon/
[multisite]: https://docs.ceph.com/en/latest/radosgw/multisite/
[nfs]: https://docs.ceph.com/en/latest/mgr/nfs/
[pools]: https://docs.ceph.com/en/latest/rados/operations/pools/
[prom]: https://docs.ceph.com/en/latest/mgr/prometheus/
[qemu]: https://docs.ceph.com/en/latest/rbd/qemu-rbd/
[rbd]: https://docs.ceph.com/en/latest/rbd/
[rbdk8s]: https://docs.ceph.com/en/latest/rbd/rbd-kubernetes/
[rgw]: https://docs.ceph.com/en/latest/radosgw/
[s3]: https://docs.ceph.com/en/latest/radosgw/s3/
[scrub]: https://docs.ceph.com/en/latest/rados/configuration/osd-config-ref/
[smb]: https://docs.ceph.com/en/latest/mgr/smb/
[teuth]: https://docs.ceph.com/en/latest/dev/developer_guide/testing_integration_tests/tests-integration-testing-teuthology-intro/
[upgrade]: https://docs.ceph.com/en/latest/cephadm/upgrade/
