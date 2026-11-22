<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# atlas-native edge profile

`deploy/helm/atlas-native/values-edge.yaml` runs atlas-native on one small node: a single pod
that is the only metadata voter and the only data node, keeps one copy of each extent, and caps
the caches that otherwise grow with the namespace.

```sh
helm install edge deploy/helm/atlas-native -n atlas-native --create-namespace \
  -f deploy/helm/atlas-native/values-edge.yaml
```

| Setting | Default | Edge | Why |
|---|---|---|---|
| `replicas`, `node.replicationFactor` | 3, 3 | 1, 1 | One node, one copy |
| `node.cacheInodes` | 262144 | 16384 | Unchanged inodes kept in memory per metadata group; the rest are read from the catalog store when needed |
| `node.storeCacheBytes` | 64 MiB | 8 MiB | The catalog store's page cache per group; the kernel's page cache sits behind it |
| `node.mallocArenaMax` | glibc default | 2 | Fewer glibc malloc arenas: less memory held by idle threads |
| `node.maxRequestBytes` | 16 MiB | 8 MiB | Requests are buffered whole, so this bounds memory under concurrent writes |
| `node.repairIntervalSecs`, `node.scrubBytesPerSec` | 300, 64 MiB/s | 86400, 8 MiB/s | With one copy a scrub detects corruption but can't repair it, so run it daily and slowly |
| `resources` | 64 MiB request, 512 MiB limit | 48 MiB request, 256 MiB limit | |

Everything else is the normal chart: the CSI driver, NFS, SMB and S3 gateways and replication can
be enabled on top. The Atlas gateway (`deploy/helm/atlas`, SQLite) runs beside it; on the lab its
pods use 8–33 MiB.

## Protecting the data

With one copy, the node's disk is the only copy of the data. A failed disk loses everything
written since the last off-site copy. Pair the profile with one of:

- **Replication to another cluster** (`replication.jobs`,
  [`NATIVE_REPLICATION.md`](NATIVE_REPLICATION.md)): a read-only replica in a core site, updated
  every interval with only the changed extents, promotable if the edge site is lost.
- **Snapshot export to S3** (`tiering`, [`NATIVE_NODE.md`](NATIVE_NODE.md)): snapshots as
  content-addressed objects, importable into a new volume on any cluster.

## Memory

Measured with a release build, files spread over 400 directories. The figure is the peak
resident memory of the catalog alone (one metadata group, checkpointed every 1 024 creates;
`BENCH_DIRS=400 BENCH_CACHE_INODES=… BENCH_STORE_CACHE=… cargo test --release -p atlas-native
--lib store::bench::memory_of_a_paged_catalog -- --ignored --nocapture`):

| Files | Default caches | Edge caches |
|---|---|---|
| 100 000 | 75 MB | 23 MB |
| 200 000 | 142 MB | 26 MB |
| 300 000 | 195 MB | 26 MB |
| 400 000 | 196 MB | 26 MB |

The default caches stop growing once they hold 256k inodes; the edge caches stop at 16k, so
memory no longer depends on the size of the namespace. Inodes outside the cache cost a read from
the catalog store (usually from the kernel's page cache) when they are next used.

A whole node pod on the lab k3s (shared HDD host, local-path volume), 50 000 empty files in 50
directories and then 64 files of 4 MiB written through the HTTP API, resident memory from
`/proc/1/status`:

| | Default | Edge profile |
|---|---|---|
| Idle after start | 7 MB | 7 MB |
| Peak during the load | 86 MB | 61 MB |
| After a restart (catalog reloaded) | 23 MB | 16 MB |

At 50 000 files the default caches aren't full yet, so the difference is small. It grows with the
namespace, as the catalog figures above show. The run was bound by the shared disk: each create
waits for an fsync, about 12 creates per second on that host.

`tests/edge.rs` runs the same single-node layout in-process with a 64-inode cache and a 1 MiB
store cache. It writes past a Raft log compaction, so later reads page inodes from the store, and
then checks every entry and file after a restart.

## Not covered yet

- No numbers on ARM boards or under a cgroup memory limit near the floor; the 256 MiB limit leaves
  headroom over the measured peak.
- A single voter has no failover: if the pod's node goes down, the filesystem is unavailable
  until it returns (or until a replica elsewhere is promoted).
- Idle CPU is about 15 millicores, from the Raft tick and background loops; no low-power tuning
  has been done.
