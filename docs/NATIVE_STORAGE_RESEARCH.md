<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# atlas-native: storage research for speed and for AI-agent workloads

Written 2026-11-28 against commit `4dc63232` (`feat/native-delta-writes`). Nothing was built, run or
benchmarked for this page.

The request is read here as: storage for AI-agent workloads, using recent research, measurably
faster, working end to end. **That reading is not confirmed.** Part 1 is general speed research, part 2
is what agents need, part 3 is what to bet on.

## How to read this

- **Sources** were fetched, and the passages quoted were read (usually abstract, design and setup, not
  every page). Figures are the authors' own, on their hardware. None was reproduced. arXiv papers are
  preprints unless a venue is named.
- ***Inferred*** marks a conclusion from reading code that no test or measurement here shows.
- Code paths are relative to `crates/`.
- Effort: **S** up to 2 weeks, **M** 1-2 months, **L** 3-6 months, for one experienced developer
  (unvalidated guesses).

## Summary

- **No research result found here changes the basic limit of the test setup:** three replicas share one
  7,200 rpm disk, and every acknowledged write waits for that disk several times. A win on it says
  little about other hardware.
- **The nearest gains look like our own serialisation, not new research.** *Inferred:* a new small file
  is written while holding a group-wide lock across both the data syncs and the Raft commit, and the
  Raft leader syncs its log only after a follower answers. Small-file creation is where atlas-native is
  furthest behind in the quick run (4.2-4.9 against 26.5 per second, one run,
  [NATIVE_DELTA_WRITES](NATIVE_DELTA_WRITES.md)), and it is not covered by the
  [delta redesign](NATIVE_DELTA_REDESIGN.md).
- **For agents the primitive exists but does not scale:** snapshot and clone copy no file data, but
  both do work proportional to the number of files.
- **Not worth doing on this host:** io_uring for speed, SPDK, ZNS or FDP, CXL, RDMA, ublk, fewer-round-trip
  consensus, and acknowledging from memory in the cluster.
- **Three bets** (part 3): concurrent small-write commits; fork cost independent of file count; an
  asynchronous small-file path for private workspaces. Each has an experiment that can kill it first.
- Nothing here promises that atlas-native will beat Ceph.

## The test host and what was measured

Read-only `lscpu`, `lsblk`, `free`, `uname`: Xeon E-2336 (6 cores, 12 threads), 31 GiB RAM, two WDC
WD10EZEX 1 TB rotational SATA disks (`sda` root, ext4; `sdb` XFS on `/data`), no NVMe device listed,
Ubuntu 26.04.1, kernel 7.0.0-44. The benchmarks put all of both systems on `/data`.

Measured elsewhere ([NATIVE_DELTA_WRITES](NATIVE_DELTA_WRITES.md)): delta writes led CephFS in a 60 s
4 KiB random-write test at queue depth 32 (56.9 against 38.7 IOPS) and lost over 20 minutes (23.8
against 37.8). With 128 KiB cells the rate was level (39.2) with 82% of folding still to do, and
sequential reads were 3 against 103 MiB/s. Disk ceiling, metrics and parallel folding are being worked
on separately and are not repeated here.

## Code facts that matter for what follows

All *inferred* from source; confirm before acting.

| # | What the code does | Where | Consequence |
|---|---|---|---|
| K1 | A write that is neither a delta nor a whole number of grid cells takes `write_lock`, then places replicas and waits for the Raft commit inside it. A new file whose size is not a whole number of grid cells (1 MiB by default) is such a write. Aligned writes place data outside the lock but commit inside it. `setattr` takes the same lock. | `atlas-native/src/engine/files.rs:504-509, 578-592`; `engine.rs:670, 774-779, 977` | At most one such commit in flight per metadata group. [NATIVE_VS_CEPH](NATIVE_VS_CEPH.md) A2 records the lock for partial overwrites. |
| K2 | `propose` appends without syncing and broadcasts. The leader syncs its log when a follower acknowledges (or at the next tick), under the node lock. | `atlas-native/src/raft.rs:926-927, 958-975, 1132, 1473-1493` | Follower sync, then leader sync, in series. Raft's described optimisation runs them in parallel ([Ongaro §10.2.1][raft]); the code comment says it does too. |
| K3 | A small file costs at least: create (3 log syncs), data (3 replica `fdatasync`s, usually on growing files), extent commit (3 log syncs). Log syncs can be shared between concurrent creates. | K1, K2, `atlas-native/src/device.rs:102-130` | Nine or more syncs on one spindle, in sequential stages. |
| K4 | Filesystem snapshot reads the whole inode table into memory and raises every extent's reference count; clone copies that tree and does the same; the clone's records are all written at the next checkpoint. Each is one replicated command. | `atlas-native/src/namespace.rs:729, 1435, 1509`; `inodes.rs:428`; `store.rs:421` | Fork time and memory grow with file count on every metadata node, probably stalling that group while it applies. |
| K5 | `open` never sets `FOPEN_KEEP_CACHE`; `close` sends buffered data and waits for its commit. | `atlas-native-fuse/src/fuse.rs:131-139, 605`; `ops.rs:1068` | Without that flag the kernel page cache is dropped on every open ([libfuse header][libfuse]). Every closed file pays a durable commit. |
| K6 | All appends to a device share one file descriptor, and each calls `fdatasync` on it. | `atlas-native/src/device.rs:102-117` | **Possible durability bug.** Linux reports a writeback error to one sync call on a descriptor; later calls return 0 ([VFS docs][vfs]). A concurrent writer whose data failed could be told it succeeded. Not reproduced. Test with fault injection whatever else is decided; branch `exp/native-group-commit` fails all uncovered writers. |

## Part 1: speed research

"Gain here" means on this single spinning disk.

### Writes and commits

**R1. Log small writes, apply later (BlueStore deferred writes; journaling file systems).**
- Idea: put small data and metadata in one sequential log, acknowledge, write in place afterwards.
- Sources and measurements: [BlueStore, SOSP'19][bluestore] (Ceph authors; 16 nodes, 7,200 rpm HDDs):
  writes below the allocation unit go to the RocksDB log first; overwrites up to 64 KiB on HDD are then
  applied in place to avoid read seeks. BlueStore as a whole gave 50-100% more steady-state object write
  throughput than the older file-system backend, and 20% more below 64 KiB in its block workload. Current
  docs still send writes below `min_alloc_size` through the [BlueStore WAL][bscfg].
  [Okeanos, ATC'11][okeanos] (modified Linux 2.6.18, 15,000 rpm SAS, journal on a second disk): packing
  sub-block writes into journal blocks almost doubled throughput over ordered mode for 1 KiB writes under
  PVFS2. [NVLog, FAST'25][nvlog] reports up to 15.09x with the log on non-volatile memory.
- atlas-native: this is phase 3 of the redesign (apply in place), plus one log per device shared by data
  and Raft (phase 4).
- Hardware: none for a same-disk log; the NVLog figures need NVM.
- Effort L. Durability risk **high**: stale replicas, torn in-place writes, replay.
- Gain here: **plausible, unproven.** The system we compare against has this mechanism for small writes
  (whether it was active in our runs was not checked). A single disk still pays for the later write.

**R2. Log-structured block layout with background collection.**
- Idea: all writes are appends; space is reclaimed by compaction.
- Sources: [Alibaba EBS, FAST'24][ebs] (production): moving to a log-structured design, with compression and
  erasure coding done in the background, cut space use from 3x to 1.29x on average, with collection write
  amplification under 1.5 on the most stressed clusters. [Pangu, FAST'23][pangu] builds on one append-only layer (SSD, RDMA).
  [DisCoGC, FAST'26][discogc] (ByteDance, SSD) reports about 20% lower cost by discarding dead ranges
  before compacting.
- atlas-native: extents are already append-only and copy-on-write. Missing: compaction and discard
  (NATIVE_VS_CEPH S4).
- Effort L. Risk medium.
- Gain here: **no.** Scattered small pieces are what already hurt reads while deltas are unfolded.

**R3. Copy only when enough has changed.**
- [How to Copy Files, FAST'20][clone] (4 GiB RAM, 7,200 rpm disk): after 16 rounds of clone plus 16-byte
  edits, cold-cache `grep` slowed 3-4x on XFS and ZFS clones; the authors buffer small changes until they
  justify a rewrite ("copy-on-abundant-write", in the BetrFS write-optimised file system).
- atlas-native: argues for folding on accumulated bytes, not delta count or age, and for measuring read
  locality after churn.
- Effort S to try as a policy. Risk low. Gain here: **possible**, for the existing fold policy only; the
  full BetrFS structure is not a realistic port.

**R4. Parallel leader log write.** See K2 and [Ongaro][raft]. No format change. Effort S. Risk low if the
leader still counts itself only after its sync returns (it already tracks `synced_index`). Gain here:
**possible but smaller than on separate disks**, since both syncs queue on one spindle.

**R5. Fewer round trips: CURP, Skyros, shared logs, fast paths.**
- Idea: make writes durable first and order them later, saving a network round trip.
- Sources: [CURP, NSDI'19][curp] (InfiniBand RAMCloud: write latency 14 to 7.1 µs, 4x throughput);
  [Skyros, SOSP'21][skyros] (five AWS m5zn bare-metal replicas: 3x lower latency than batched Paxos for
  interfaces that reveal nothing in the reply); [LazyLog, SOSP'24][lazylog] and [SpecLog, OSDI'25][speclog]
  (delayed or speculated global order across shards; SpecLog: 1.6x lower end-to-end latency than Scalog);
  [BtrLog, VLDB'26][btrlog] (three local-SSD log nodes: EBS io2 median latency 4-5x higher);
  [Jetpack, OSDI'26][jetpack] (geo-distributed: average commit latency down by up to 60%).
- atlas-native: new commit protocol, view-change and recovery rules. Effort L. Risk **very high**.
- Gain here: **none.** Loopback round trips are tiny next to a disk flush, and these save round trips, not
  flushes. Revisit with multi-host NVMe.

**R6. Acknowledge from memory while all replicas are up.**
- [SAUCR, OSDI'18][saucr]: on HDDs, forced writes cost about 50x in their ZooKeeper test; buffering in
  memory while enough nodes are up and flushing when they fail gained 25-100x on HDDs. It relies on replicas
  rarely failing at the same instant; if a majority do, it stays safe by remaining **unavailable**.
- Gain here: large, but **do not**: on one host a power cut hits all three replicas at once, which is exactly
  that case, and the buffered data is gone.

**R7. Own the log device.** BlueStore reports that on a journaling file system each `fsync` issues two
cache flushes, and that raw-disk object creation was 80% faster than XFS on HDD
([paper][bluestore]). Preallocated, reused log files remove part of that without leaving XFS. Effort S for
preallocation, L for raw devices (the `aligned` backend exists but was no faster in one HDD run,
[NATIVE_PERF](NATIVE_PERF.md)). Gain here: **unknown**; the disk-ceiling measurement (appended against
preallocated) answers it. Any change to sync handling must respect [fsync failure behaviour][fsync]
(ATC'20: ext4, XFS and Btrfs mark pages clean after a failed `fsync`, so retrying proves nothing).

### Metadata, small files, cloning, many readers

**R8. Asynchronous namespace operations.**
- The Linux CephFS client can complete namespace operations before the MDS replies when it holds the
  needed capabilities (`nowsync`, [default since 5.16][mountceph]); the MDS [journals metadata events in
  batches][mdsj]. The benchmark uses `ceph-fuse`; whether it does the same was not checked.
- atlas-native: let the mount continue before a commit, with an explicit barrier. Effort M-L. Risk medium:
  a client crash loses files it had closed. Gain here: **possible** through batching, if the server commits
  concurrently (needs K1 fixed). Bet 3.

**R9. Server-side metadata scaling.** [InfiniFS, FAST'22][infinifs] (partitioned directory tree, up to
100 billion files) and [FalconFS, NSDI'26][falcon] (13 dual-socket NVMe machines, 100 GbE: up to 5.72x
the small-file throughput of CephFS and Lustre, with server-side path resolution and request merging).
Effort L. Gain here: **no** from sharding; request merging is the same idea as group commit (bet 1).

**R10. Constant-cost branching.**
- [BranchFS][branchfs] (workshop paper; local FUSE, median of 10, daemon-internal time): branch creation
  292-317 µs for base directories of 100 to 10,000 files, because a branch is an empty delta layer. It treats `fsync` as a
  no-op until commit.
- [DeltaBox, ATC'26][deltabox] (96-core server, NVMe): checkpoints by switching overlayfs layers over XFS
  reflink; mean rollback 1.86 ms.
- atlas-native: a clone becomes a parent pointer plus a delta layer instead of a copied inode table. The
  [snapshot paging note](NATIVE_SNAPSHOT_TREE_PAGING.md) proposes versioned records, which covers
  snapshots; clones also need lookups that fall through to the parent. Effort L. Risk **high**: metadata
  format, and a lost catalog cannot be rebuilt from data devices (NATIVE_VS_CEPH F5).
- Gain here: **yes in principle** (it removes work, whatever the disk). Bet 2.

**R11. Shared content-addressed caches and lazy loading.**
- [AWS Lambda, ATC'23][lambda] (production): about 80% of newly uploaded functions had no unique chunks;
  for one week in a large region the median share of chunks served was 67% from the worker cache, 32% from
  a zone cache and 0.06% from the origin store. [REAP][reap] (local SSD): prefetching the recorded working
  set cut snapshot cold starts 3.7x. [FlacIO, FAST'25][flacio], [CoFS, FAST'26][cofs] (FUSE lookups up to
  86% faster than a FUSE loopback) and [ThinkAhead, FAST'26][thinkahead] attack the same startup cost.
- atlas-native: extents are immutable and carry SHA-256, which suits a node-wide cache shared by all
  mounts. Today each pod has its own mount and the page cache is dropped on open (K5). Effort S to keep the
  cache under cache leases, M for a node-wide cache. Risk: stale reads if invalidation is wrong.
- Since: keeping the cache exists as an opt-in, `atlas-native-mount --keep-cache`, which compares a content
  tag at each open and does not use cache leases ([NATIVE_FS](NATIVE_FS.md), "Keeping the page cache"). Its
  speed has not been measured; there is no node-wide cache.
- Gain here: **little**: data nodes are on the same host and already read through the page cache. It
  matters once data is remote. Phase 3 of the redesign would end extent immutability for some extents.

### The I/O path

| # | Candidate and source | What it was shown on | Change, effort | Gain here |
|---|---|---|---|---|
| R12 | io_uring ([Jasny et al., VLDB'26][iouring]) | NVMe and fast NICs. `fsync` runs on fallback worker threads; consumer SSDs stay dominated by millisecond `fsync`; linking write and `fsync` did not help | Backend exists (`io-uring` feature), never produced a result here. S to measure | **No** for durable writes on HDD |
| R13 | FUSE: [passthrough][fusepass], [io_uring transport][fuseuring], [RFUSE, FAST'24][rfuse]; cost study [FAST'17][tofuse] (-83% to +6% against ext4 by workload and device) | BranchFS: passthrough reads 7,236 MB/s against 1,655 normal and 8,800 native | Passthrough needs a local backing file and `CAP_SYS_ADMIN`; RFUSE needs its own kernel driver; [writeback cache is documented][fuseio] as generally unsuitable for network file systems. M-L | **No** while the disk is the limit. Only keep-cache (R11) is cheap |
| R14 | [ublk][ublk] block devices, with zero-copy options | Kernel documentation | `atlas-native-ublk` exists; functional checks passed on kernel 7.0 on 2026-11-28, speed not measured ([NATIVE_UBLK](NATIVE_UBLK.md)) | **No** speed gain on files; needed for block volumes, which is a different goal |

### Hardware-dependent work

| # | Candidate and source | Shown on | Verdict |
|---|---|---|---|
| R15 | [ZNS, ATC'21][zns] (RocksDB write throughput 2x, p99.9 random read 2-4x lower); [FDP, EuroSys'25][fdp] (device write amplification about 1 in CacheLib) | Zoned and FDP SSDs | Cannot help: no such device. Revisit only with a measured write-amplification problem |
| R16 | RDMA and SPDK-style user-space stacks ([Pangu RDMA, NSDI'21][rdma]; [Pangu 2.0][pangu], 100 µs-level I/O) | SSD fleets, RDMA fabric | Cannot help here. atlas-native RDMA is SoftRoCE-verified plumbing only ([STATUS](STATUS.md)) |
| R17 | CXL memory pooling ([Pond][pond], 7% lower DRAM cost within 1-5% performance) | Pooled memory for cloud VMs | Not a storage speed technique for this engine. **Not worth it** |
| R18 | Disaggregated or object-backed logs (EBS3, BtrLog above) | Cloud fleets | Needs several hosts. No gain here |

Not opened, so not relied on: Ursa (block storage with SSD primaries and journalled HDD backups, as
described in search results only; the ACM page for its 2023 journal version returned 403 and a guessed
author copy did not exist).

## Part 2: AI-agent workloads

### What the sources say

- Startup is a large cost. In [AgentCgroup][agentcgroup] (144 tasks, Claude Code in Podman, images
  2.9-17.3 GB), container and agent initialisation took 31-48% of the task lifecycle, mostly remapping file
  ownership across image layers (a metadata walk), and package installation was about 10% of shell-command
  time.
  [Rollout infrastructure tax][rollout] (authors from a sandbox vendor) found cold start varied up to 110x
  between substrates.
- Search and RL fork sandboxes in numbers from tens to low hundreds and checkpoint often
  ([DeltaBox][deltabox], which quotes 840-1,072 ms checkpoints for two microVM sandboxes, mostly VM state).
- Most turns need no checkpoint: over 75% produced no recovery-relevant state, and ZFS snapshots stayed
  within 22 ms under concurrency on NVMe ([Crab][crab]).
- Sessions sit idle for minutes to hours and can hold large state (sandbox memory peaks of 28 GB,
  [AgentSysBench][agentsys]).
- Agents damage files; [YoloFS, SOSP'26][yolofs] studies 290 public reports and asks the file system for
  staging, undo and access gates.
- Model side: KV-cache pooling over DRAM, SSD and RDMA raised effective request capacity 59-498%
  ([Mooncake, FAST'25][mooncake]); a loading-optimised format loaded models 3.6-8.2x faster
  ([ServerlessLLM, OSDI'24][sllm]); [ByteCheckpoint, NSDI'25][bytecp] cut checkpoint stalls 54x on average;
  DeepSeek's [3FS notes][threefs] put their FUSE limit near 400,000 4 KiB reads per second, so
  performance-critical clients use a native zero-copy API.

### What atlas-native has

| Need | What exists | Verified? | Gap |
|---|---|---|---|
| Fork a workspace | Filesystem snapshot and clone, no data copied ([NATIVE_FS](NATIVE_FS.md)) | Isolation checked over FUSE, three nodes on one host, 2026-10-04. Fork time **not measured** | Cost grows with files (K4); snapshot trees stay in memory |
| Lifecycle per run | Workspace create, reset, discard, TTL, quotas; REST and MCP (`atlas-gateway/src/routes/workspaces.rs`) | Fixture driver only; **not run on a cluster** | Reset is delete plus clone; no commit back to the source. A [change listing](NATIVE_FS.md#what-changed) (REST, MCP, `atlasctl`) has in-process tests only, not run on a cluster or measured |
| git and small files | POSIX filesystem | pjdfstest, `git clone` and `fsck` on the lab (2026-10-04) | Creates about 5-6x slower than CephFS in one run (K1-K3) |
| Many sandboxes | CSI driver, one mount per pod ([NATIVE_CSI](NATIVE_CSI.md)) | Single-node k3s | No node-wide cache (K5); mounts break if the node plugin restarts |
| Isolation | Per-filesystem byte and inode quotas; gateway tenant checks | Unit and integration tests | One shared node API token (NATIVE_VS_CEPH O4); no per-workspace I/O limit found |
| Undo and protection | Snapshots, retention locks ([WORM](WORM.md)) | Locks checked on the lab | No per-operation staging or approval in the filesystem |
| Process memory state | None | n/a | Out of storage's scope; needs a sandbox runtime |
| Training checkpoints | Whole-extent fast path; `--direct-writes`; `checkpoint seal` | Direct writes in-process only; seal never run on real training output | Needs several NVMe hosts to judge |
| Model and dataset reads | Parallel extent reads, read-ahead, direct reads, locality pin | Throughput measured on tmpfs and one shared HDD only ([NATIVE_PERF](NATIVE_PERF.md)) | Whole-extent verification on small random reads; GPUDirect never run |
| KV-cache offload | Recipes over mount and S3; `KVConnector` glue ([AI_INFERENCE](AI_INFERENCE.md)) | **Not run** with any inference engine | No native client API. Not worth work without GPUs and NVMe |

### Benchmark: agent profile against CephFS (proposed, not written or run)

Add an `agent` profile to `deploy/bench-vs-ceph/`, keeping its rules: one system at a time under
`bench.lock`, same container limits, three replicas, same disk, FUSE on both sides.

Setup:
- Add `git` and `tar` to both images. The Atlas container has no network, so generate fixtures once on the
  host from a fixed seed, record their SHA-256, and mount them read-only.
- `pkgs.tar`: 2,000 files by default (`AGENT_FILES`), about 20 per directory; sizes 70% 200 B-4 KiB, 25%
  4-32 KiB, 5% 32-256 KiB. These proportions are a chosen stand-in, not measured agent traffic.
- `repo.git`: bare repository, 1,000 files, 20 commits, fixed authors and dates.
- `manifest.sha256` for every expected file.

Fixture generator: a script under `deploy/bench-vs-ceph/` seeded with `AGENT_SEED=1`; paths
`d%03d/f%05d`, contents from the seeded generator, so a rerun gives the same hashes. The core of phases A to
C, identical on both mounts (`$WS` is an empty directory on the mount):

```sh
tar -xf /fixtures/pkgs.tar -C "$WS"                                          # A, tool exit
find "$WS" \( -type f -o -type d \) -print0 | xargs -0 -n1 -P8 sync           # A, durable
git clone -q --no-hardlinks /fixtures/repo.git "$WS/src"                     # B
git -C "$WS/src" status --porcelain
git -C "$WS/src" ls-files | LC_ALL=C sort | head -20 |
  while read -r f; do echo bench >>"$WS/src/$f"; done
git -C "$WS/src" -c user.name=bench -c user.email=bench@invalid commit -qam bench
git -C "$WS/src" fsck --full
(cd "$WS" && sha256sum --quiet -c /fixtures/manifest.sha256)                 # C
```

Fork calls for phase D (each followed by mounting the clone and reading one fixed file):

```sh
# atlas-native, on the metadata leader, then atlas-native-mount --fs c1
POST /v1/fs/bench/snapshots      {"id": "s1", "name": "s1"}
POST /v1/fs-snapshots/s1/clone   {"id": "c1", "name": "c1"}
# CephFS, base tree inside subvolume "base"; poll status until "complete", then mount its path
ceph fs subvolume snapshot create bench base s1
ceph fs subvolume snapshot clone bench base s1 c1
ceph fs clone status bench c1
```

| Phase | Steps | Record |
|---|---|---|
| A install | Unpack into a new workspace, then `fsync` every file and directory | Time to tool exit **and** time to durable. Both are needed because the two systems may differ on what `close` guarantees |
| B git | Clone, status, 20 fixed edits, commit, `fsck` | Time per command |
| C verify | `sha256sum -c`; `kill -9` the mount after A's barrier, remount, verify again | Pass or fail (this is not a power-loss test) |
| D fork | Base trees of 1k, 10k, 100k files; snapshot, writable clone, mount, first read; repeat with one client writing in the background | Fork time, foreground p99 during it, metadata RSS and disk bytes, Raft elections |
| E fan-out | 1, 4, 16 clones at once: full-tree read cold, then warm, then 20 edits each; check source and siblings unchanged | Wall time, slowest clone, p99, disk bytes |
| F discard | Delete the clones and snapshots; sample space until stable | Time, space reclaimed |
| G large files | 4 x 256 MiB write, rename, `fsync`; drop server caches; read | MiB/s |

Comparisons for D and E, each labelled with its guarantee:
1. atlas-native clone against CephFS `subvolume snapshot clone`, polled until complete. Ceph documents this
   as [an asynchronous bulk copy][fsvol]. The current Ceph script would need the base tree in a subvolume.
2. Both systems' read-only snapshot with a local overlayfs upper layer (upper layer unreplicated). This is
   Ceph's strongest cheap fork and must be included. Overlayfs over these FUSE mounts is untested.
3. Floor: `cp -a --reflink=always` on the host XFS (one copy; check reflink is enabled).

Reporting: three alternating repeats, median and range, load and I/O pressure at start and end; any
verification failure fails the run. At today's create rate 2,000 files take roughly 8 minutes in phase A
(arithmetic). Build the large phase D trees on tmpfs-backed server data and say so, or they take hours.

## Part 3: ranked bets

Do the experiment before the code. Stop at the first failure.

### Bet 1: let small writes commit concurrently, and sync the leader log in parallel

- **Claim:** in the existing `smallfile-create-4k` job (8 writers, `fsync` on close, three copies, same
  disk), at least **3x** today's atlas-native rate, with the same durability for every acknowledged call.
  Whether that passes CephFS is for the run to show.
- **Change:** place new or fully replaced extents outside `write_lock` and commit them as a compare-and-swap
  on the cell's current extent, as folding already does; order only writes that touch the same cell or
  change the file's size; sync the leader log from its own thread.
- **Status of the concurrent commits:** implemented, **off by default, not measured**
  (`metadata.concurrent_commits`, [NATIVE_METADATA](NATIVE_METADATA.md#concurrent-commits-opt-in)).
  The compare-and-swap is the leader's check at proposal time, not a new log record. Partial overwrites
  still hold the write lock, so the gain predicted for them below is not expected from this. The
  disproof experiment has not been run.
- **Cheapest disproof (days, no change to the write path):** run the job with 1, 8 and 32 writers and read the
  new sync metrics. The bet is dead if the rate already scales with writers, or if one writer already keeps
  the disk at the measured flush ceiling with few syncs to remove. Then a throwaway spike on an in-process
  cluster on the HDD: under 1.5x, stop.
- **Hardware:** current host. **Effort:** S for the log sync, M for the lock.
- **Risk:** ordering bugs between concurrent writes, truncate, snapshot and failover. Needs the model-based
  random tests and halted-leader tests the delta path has. Check K6 first: more concurrent syncs make it
  matter more.
- **Also helps** ordinary partial overwrites with delta writes off (3.8 IOPS in the quick run). It is not
  folding work.
- **Status of the log sync half:** implemented, **off by default, not measured**
  (`metadata.parallel_log_sync`, [NATIVE_METADATA](NATIVE_METADATA.md#parallel-leader-log-sync-opt-in)).
  The disproof experiment above has not been run.

### Bet 2: make fork cost independent of file count

- **Claim:** snapshot plus writable clone in **under 1 s** at 100,000 files, time and metadata-node memory
  growth within 2x from 1,000 to 100,000 files, and no election or doubling of foreground p99 meanwhile.
- **Cheapest disproof:** time today's snapshot and clone at 1k, 10k and 100k files on tmpfs (benchmark
  phase D, or an engine test). If today already meets the claim at the sizes agents will use, **do
  nothing**. A CephFS comparison alone does not justify it: Ceph's writable clone copies data, so today's
  metadata-only clone may already win.
- **Change:** versioned records ([paging note](NATIVE_SNAPSHOT_TREE_PAGING.md)) plus parent-pointer clones
  with tombstones; release shared extents per layer.
- **Hardware:** current host. **Effort:** L.
- **Risk:** the highest of the three: catalog format change with no downgrade, collection errors lose data,
  and lookups through deep clone chains slow down unless layers are merged.

### Bet 3: asynchronous small-file path for private workspaces (only after bet 1)

- **Claim:** for a workspace with one writer mount, "unpack the fixture, then one durability barrier" takes
  at most **one third** of the time of the same mount today. Everything covered by the barrier is as durable
  as today.
- **Change:** in the mount only, opt-in per workspace: return from `create` and `close` before the commit,
  keep per-directory order, send in parallel, report errors at the barrier (`fsync` still waits). Raft and
  data nodes are unchanged.
- **Cheapest disproof:** after bet 1, run phase A with 64 independent writers. If total throughput is not at
  least 3x that of one writer, the server cannot batch enough: drop it.
- **Hardware:** current host. **Effort:** M.
- **Risk:** files a program closed are lost if the mount or host dies before the barrier; other mounts do not
  see them until then; quota errors arrive late. Never the default, and a product decision. Do **not**
  copy BranchFS in ignoring `fsync`.

### Not bets

| Item | Verdict |
|---|---|
| Sustained random writes and reads after folding | Already planned. Research supports log-then-apply in place (R1) and byte-based fold thresholds (R3). If the disk-ceiling result shows syncs are the limit, a shared per-device log (redesign phase 4) is the next candidate |
| Faster consensus, memory acknowledgement (R5, R6) | Not worth it: no gain, or unsafe on one host |
| io_uring, FUSE transport changes, ublk (R12-R14) | Not worth it for speed on this disk |
| ZNS, FDP, CXL, RDMA, GPUDirect, KV-cache connector | Wait for hardware and a user who needs it |
| Node-wide chunk cache (R11) | Later, with more than one host; keep-cache is a cheap earlier step |
| Process checkpoint and restore | Leave to the sandbox runtime; offer a fast filesystem snapshot it can call |
| Doing nothing about fork | Right answer if bet 2's first measurement passes |

A bet that passes on this host still needs NVMe and at least four hosts before it supports any general
claim against Ceph.

[agentcgroup]: https://arxiv.org/pdf/2602.09345
[agentsys]: https://arxiv.org/pdf/2608.15127
[bluestore]: https://www.pdl.cmu.edu/PDL-FTP/Storage/ceph-exp-sosp19.pdf
[branchfs]: https://arxiv.org/pdf/2602.08199
[bscfg]: https://docs.ceph.com/en/latest/rados/configuration/bluestore-config-ref/
[btrlog]: https://arxiv.org/pdf/2606.27051
[bytecp]: https://arxiv.org/pdf/2407.20143
[clone]: https://www.usenix.org/system/files/fast20-zhan.pdf
[cofs]: https://www.usenix.org/system/files/fast26-wang-li.pdf
[crab]: https://arxiv.org/pdf/2604.28138
[curp]: https://www.usenix.org/system/files/nsdi19-park.pdf
[deltabox]: https://arxiv.org/pdf/2605.22781
[discogc]: https://www.usenix.org/system/files/fast26-bian.pdf
[ebs]: https://www.usenix.org/system/files/fast24-zhang-weidong.pdf
[falcon]: https://www.usenix.org/system/files/nsdi26-xu.pdf
[fdp]: https://arxiv.org/pdf/2503.11665
[flacio]: https://www.usenix.org/system/files/fast25-liu-yubo.pdf
[fsvol]: https://docs.ceph.com/en/latest/cephfs/fs-volumes/
[fsync]: https://www.usenix.org/system/files/atc20-rebello.pdf
[fuseio]: https://docs.kernel.org/filesystems/fuse/fuse-io.html
[fusepass]: https://docs.kernel.org/filesystems/fuse/fuse-passthrough.html
[fuseuring]: https://docs.kernel.org/filesystems/fuse/fuse-io-uring.html
[infinifs]: https://www.usenix.org/system/files/fast22-lv.pdf
[iouring]: https://arxiv.org/pdf/2512.04859
[jetpack]: https://www.usenix.org/system/files/osdi26-tang.pdf
[lambda]: https://www.usenix.org/system/files/atc23-brooker.pdf
[lazylog]: https://ramalagappan.github.io/pdfs/papers/lazylog.pdf
[libfuse]: https://raw.githubusercontent.com/libfuse/libfuse/master/include/fuse_common.h
[mdsj]: https://docs.ceph.com/en/latest/cephfs/mds-journaling/
[mooncake]: https://www.usenix.org/system/files/fast25-qin.pdf
[mountceph]: https://docs.ceph.com/en/latest/man/8/mount.ceph/
[nvlog]: https://www.usenix.org/system/files/fast25-wang.pdf
[okeanos]: https://www.usenix.org/legacy/event/atc11/tech/final_files/Hatzieleftheriou.pdf
[pangu]: https://www.usenix.org/system/files/fast23-li-qiang_more.pdf
[pond]: https://arxiv.org/pdf/2203.00241
[raft]: https://web.stanford.edu/~ouster/cgi-bin/papers/OngaroPhD.pdf
[rdma]: https://www.usenix.org/system/files/nsdi21-gao.pdf
[reap]: https://arxiv.org/pdf/2101.09355
[rfuse]: https://www.usenix.org/system/files/fast24-cho.pdf
[rollout]: https://arxiv.org/pdf/2607.01415
[saucr]: https://www.usenix.org/system/files/osdi18-alagappan.pdf
[skyros]: https://par.nsf.gov/servlets/purl/10299722
[sllm]: https://www.usenix.org/system/files/osdi24-fu.pdf
[speclog]: https://www.usenix.org/system/files/osdi25-bhat.pdf
[thinkahead]: https://www.usenix.org/system/files/fast26-chen.pdf
[threefs]: https://raw.githubusercontent.com/deepseek-ai/3FS/main/docs/design_notes.md
[tofuse]: https://www.usenix.org/system/files/conference/fast17/fast17-vangoor.pdf
[ublk]: https://docs.kernel.org/block/ublk.html
[vfs]: https://docs.kernel.org/filesystems/vfs.html
[yolofs]: https://arxiv.org/pdf/2604.13536
[zns]: https://www.usenix.org/system/files/atc21-bjorling.pdf
