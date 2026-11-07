<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Atlas Native storage engine — Phase 1

This PR introduces the first native Atlas data-plane primitive: an append-only replicated extent engine.
It intentionally does **not** claim WEKA-equivalent maturity. The purpose of Phase 1 is to establish durable
extent, checksum, placement, snapshot and telemetry contracts that later distributed metadata, RDMA, SPDK,
GDS and erasure-coding work can build on.

## Invariants

- Writes allocate new extents; existing extents are never overwritten in place.
- Every extent is checksummed with SHA-256.
- Placement is failure-domain aware and defaults to 3 replicas on distinct hosts/racks. With
  `metadata.erasure` set (`4+2`, `8+3`, any k+m up to 32+8), extents of at least
  `erasure_min_bytes` are Reed-Solomon coded into k data and m parity shards on k+m distinct
  nodes instead (`docs/NATIVE_NODE.md`, "Erasure coding").
- Metadata is checkpointed incrementally into `catalog.redb` (one durable redb transaction per checkpoint).
- Snapshot creation copies only extent references, so subsequent writes are copy-on-write.
- Reads validate checksums and may fall back to another healthy replica; a read starts at a
  replica chosen from the extent id, so reads of many extents spread over all replicas.
- Runtime counters record IOPS/bytes/checksum failures/replica fallbacks.
- `atlas_native_io.bpf.c` defines the bounded eBPF ABI for block latency attribution; `atlas-io`
  loads it live (`/io/native`, maps pinned under `/sys/fs/bpf/atlas/native`, `docs/IO_EBPF.md`).

## Status against the roadmap

The roadmap and the comparison it serves are in [`COMPARISON.md`](COMPARISON.md).

| Step | Status |
|---|---|
| Metadata WAL, one Raft group, extent refcounts and GC, repair | Done (`docs/NATIVE_NODE.md`) |
| POSIX namespace and FUSE client | Done (`docs/NATIVE_FS.md`) |
| Data path: parallel extent I/O, group commit, pooled data-node connections, direct client reads | Done (`docs/NATIVE_FS.md`, "Data path") |
| `io_uring` device backend with `O_DIRECT` on raw NVMe, multi-device striping | Done (`docs/NATIVE_NODE.md`, "Devices"); verified on ext4, not yet benchmarked on NVMe |
| Namespace sharded across Raft groups, on-disk catalog, client leases | Started: commits cost one fsync, Raft replication is pipelined, and checkpoints write only changed records to an embedded KV store (`catalog.redb`; `metadata_bench`: ~9k creates/s from one proposer and ~12k/s from eight on a 3-voter group at 20k files). Inode tables are paged from the store through a bounded cache, and so are directory entries, one name or one page at a time, so memory no longer holds the whole namespace (filesystem snapshot trees still do); volumes and filesystems shard across Raft groups that share voters and data nodes (`metadata.groups`), with ReadIndex barriers so any replica routes correctly; client sessions (leases) hold cross-mount `fcntl`/`flock` locks that survive leader failover and are released when a client stops renewing; cache leases recalled before every change let mounts cache attributes and names without ever serving them stale (`--cache-leases`) |
| Erasure coding, rebuild controller, S3 tiering of cold extents | Done: Reed-Solomon k+m extents (`metadata.erasure`), read around lost shards and rebuilt onto spare nodes by repair; codec ~1.5 GiB/s encode and ~2 GiB/s two-shard rebuild per core (4 MiB extents). A rebuild controller on each group leader probes nodes and rebuilds the extents of nodes lost for `rebuild_delay_secs` (least spare redundancy first, paced), then scrubs incrementally. Cold extents tier to S3 (Ceph RGW by default, or any S3-compatible bucket) and are read back from their objects; verified against Ceph RGW on the Rook lab. Snapshots export to the bucket as content-addressed blobs (later exports upload only changed extents) and import into a new volume on any cluster with the bucket |
| RDMA transport, GPUDirect Storage, checkpoint fast path, CSI driver | Started: checkpoint fast path (whole-extent writes place data without the engine write lock through a shared in-flight allocator, concurrent appends on file devices, background parallel write-back in the FUSE client; `docs/NATIVE_FS.md`) and the CSI driver (PVCs as filesystems, VolumeSnapshots, snapshot and PVC clones, every access mode through per-pod FUSE mounts, the PVC size enforced as a filesystem quota with online expansion, volume stats; verified on a single-node k3s, `docs/NATIVE_CSI.md`) and the dataset locality API (per-node and per-host bytes of a tree, pinning a full copy of a dataset onto chosen hosts, host-preferring direct reads; `docs/NATIVE_FS.md`, "Dataset locality"). RDMA client side: pooled RDMA connections with TCP fallback and write grants, and `atlas-native-mount --rdma` are built (`docs/NATIVE_FS.md`, "RDMA to the data nodes"); tested over a fake transport and compiled under the `rdma` feature, not yet run over SoftRoCE or hardware, and with no pipelining or zero-copy. RDMA hardware verification and GPUDirect Storage need the NVMe/RDMA lab |
| NFS, SMB and S3 front ends, POSIX ACLs, quotas, `O_DIRECT` | Started: per-filesystem byte and inode quotas, enforced at commit on net growth and reported through FUSE `statfs` (`EDQUOT` from the FUSE client; `docs/NATIVE_FS.md`, "Quotas"), and POSIX ACLs enforced by the kernel through `--acl` mounts with mode sync and default-ACL inheritance in the cluster (`docs/NATIVE_FS.md`, "POSIX ACLs"; verified with `setfacl` on the lab k3s), and `O_DIRECT` (page-cache and client-cache bypass, write-through; `docs/NATIVE_FS.md`, "Direct I/O"; verified with `dd` across two mounts on the lab k3s), and an NFS gateway (NFS-Ganesha over FUSE mounts; NFSv4.1/4.2 and NFSv3 with NLM, cross-protocol locks, handles stable across restarts; `docs/NATIVE_NFS.md`; verified on the lab k3s), and an S3 gateway (s3s over the cluster API; buckets are filesystems or snapshots, objects are the same files mounts and NFS see, atomic PUT, multipart state in the filesystem so replicas scale out, per-key bucket grants; `docs/NATIVE_S3.md`; verified with aws-cli on the lab k3s), and an SMB gateway (Samba over FUSE mounts; SMB 2.1/3.x, local users mapped to Unix accounts so mode bits apply, oplocks and leases off for consistency with other clients, cross-protocol locks; `docs/NATIVE_SMB.md`; verified with `smbclient` and `mount.cifs` on the lab k3s), and an IO500 harness (`Dockerfile.io500`, `scripts/io500-native-remote.sh`; the full suite runs end to end on the lab k3s, `docs/IO500.md`). IO500 submission blocked on the benchmark hardware: the lab run is functional only, bound by the shared disk's sync latency |
| Asynchronous replication to a second cluster, failover and failback | Done: snapshot increments sent per inode with only changed grid cells, a read-only replica that resumes cut-short rounds, promote and demote for failover and failback, `atlas-native-replicate` and the chart's `replication.jobs` (`docs/NATIVE_REPLICATION.md`; verified live between two clusters on the lab k3s) |
| Single-node, low-memory edge profile | Done: `values-edge.yaml` (one pod, one copy, inode and catalog-store caches configurable through `metadata.cache_inodes` and `metadata.store_cache_bytes`, capped at 16k inodes and 8 MiB); catalog memory flat at ~26 MB from 200k to 400k files where the defaults reach ~196 MB, and a 61 MB peak in a 50k-file lab run (`docs/NATIVE_EDGE.md`) |
| libbpf/aya loader for the native eBPF maps | Done — `atlas-io` live mode, verified against a real native cluster (`docs/IO_EBPF.md`) |
