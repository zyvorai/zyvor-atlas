<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# atlas-native delta writes

Without this mode a partial overwrite holds the metadata group's write lock, reads the whole
extent, writes it again to every replica and commits, one write at a time. With
`metadata.delta_writes.enabled` a small overwrite instead stores only its bytes, as a **delta**
extent listed beside the cell's base extent, and many writers share one metadata commit. A
background **fold** later rewrites the base with its deltas applied.

Off by default. No performance result is claimed here: see
[What is verified](#what-is-verified-and-what-is-not).

## Which writes become deltas

A write is a delta when all of these hold. Anything else takes the previous path, unchanged.

- it lies inside one grid cell, over part of that cell's existing base extent, and ends within
  the volume or file (so size, quota, usage and holes never change on a delta write);
- it is not a rewrite of the whole base extent;
- it is at most `max_bytes` long;
- the target is not a replica filesystem;
- every metadata node the leader replicates to has advertised support (below);
- enough data nodes serve only `file` devices (block-aligned backends would lose their padding);
- the caps below have room, and no snapshot of the target is being taken.

## How it works

- A delta is an ordinary replicated, checksummed extent, so scrub, repair, rebuild and GC
  handle it. It is never compressed, erasure-coded, tiered or deduplicated.
- The writer takes the leader term (`leader_ready`, once per term), places the bytes without the
  write lock, then queues the delta. One writer at a time proposes everything queued as one
  command, under the write lock only for the proposal, leaving out writes that became stale
  (file unlinked or cut, another term).
- Each write ends **applied**, **rejected** (certainly not applied; it is retried or rewrites
  the cell) or **unknown** (timeout, lost leadership, shutdown, a failed log write or a panic
  while committing). Unknown is returned as HTTP 503, is not retried by the engine, and keeps
  the data's space reserved for 300 s. The FUSE client retries 503; a repeated write of the same
  bytes at the same offset is harmless, and log order keeps an earlier unknown command from
  landing on top of a later write.
- Reads lay the deltas over the base from one look at the catalog. A checksum failure looks the
  whole cell up again.
- `file_layout` lists no replica for a cell with deltas, so clients that read data nodes
  directly (older ones too) read that cell through the leader.
- A **fold** is a compare-and-swap: it applies only if the base is unchanged and the deltas it
  read are still the oldest ones, and keeps newer ones. It changes neither times nor usage.
- Snapshots (volume and filesystem) fold their target first: once without the lock, then with
  new deltas on the target blocked and under the write lock. So snapshots, clones, export and
  replication never see a delta. A retried snapshot that already exists still succeeds.
- Truncating into a cell with deltas folds it into the shortened extent. A snapshot or truncate
  that meets deltas anyway (a change of leader in between) is refused with a retryable 503.
- Dead extents are reclaimed only after they have been dead for `gc_grace_ms`, as this leader
  saw it (a new leader starts the clock again), in batches of up to 1024 per command.

## Configuration

In the `metadata` section of the node config. Unknown fields are rejected. The defaults are
starting points that have not been tuned.

```json
"delta_writes": { "enabled": true, "max_bytes": 131072 }
```

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Store eligible overwrites as deltas. |
| `max_bytes` | 131072 | Largest write stored as a delta. |
| `max_deltas_per_cell` | 32 | At the limit a write rewrites the cell, folding it. |
| `max_pending_deltas` | 65536 | Pending deltas per metadata group. Bounds catalog memory and the dirty-cell index written at every checkpoint. |
| `max_pending_bytes` | 2 GiB | Device bytes (over all replicas) pending deltas plus dead, unreclaimed extents may take, per group. |
| `fold_headroom_bytes` | 512 MiB | Part of that left free for folding; delta writes stop at the difference. |
| `fold_after_deltas` | 8 | A cell with this many deltas is due for folding. |
| `fold_after_ms` | 5000 | So is a cell dirty this long. |
| `fold_interval_ms` | 200 | Time between folding passes. |
| `fold_cells_per_pass` | 8 | Cells folded per pass at most: with the interval, the bound on folding (40 cells/s by default). |
| `gc_grace_ms` | 10000 | How long an extent stays dead before reclaim. |

When pending plus dead bytes pass half of the room, every dirty cell is due and a folding pass
also runs GC. The grace period still applies, so under a sustained load faster than folding and
reclaim, writes fall back to the previous path until room returns. The fallback creates more
dead data, not less, so throughput then drops to roughly that of the mode off.

With `enabled` off, deltas written earlier are still read, and the leader folds them in the
background.

## Upgrading and downgrading

- Each metadata node advertises support in its Raft replies. A leader writes deltas, and uses
  the batched reclaim, only once every node it replicates to has advertised it in the current
  term. An older node therefore keeps the mode inactive, and so does a node that is down when
  leadership changes: until it answers the new leader, writes take the previous path.
- With the mode never enabled, no new command and no new field is written to the log or the
  catalog store, and data nodes are sent no new request. (Raft replies carry one new field,
  which older builds ignore.)
- **Do not downgrade, or add a metadata node running an older build, while the log or the catalog
  may hold deltas.** Older builds cannot decode the new commands and silently drop the new
  catalog fields, which would lose acknowledged writes. Before a downgrade: turn the mode off,
  wait for `atlas_native_delta_pending` to reach 0, and let the Raft log compact past the last
  delta command. Atlas does not enforce this; a downgrade has not been tested.
- A data node from before this change cannot say whether its devices pad writes; it gets no
  deltas. The leader asks again after 60 s.

## Metrics

`atlas_native_delta_writes_total`, `atlas_native_delta_folds_total`,
`atlas_native_delta_fallbacks_total` (eligible writes that rewrote their cell instead),
`atlas_native_delta_dirty_cells`, `atlas_native_delta_pending`,
`atlas_native_delta_pending_bytes`, `atlas_native_dead_bytes`.

## Known costs and limits

- A cell with `n` deltas costs `n + 1` data-node reads, and the whole base is still read for
  any read of the cell.
- Each delta is a catalog extent record, and each append rewrites its file's inode record at the
  next checkpoint.
- Delta commits wait for the write lock, so a large unaligned rewrite delays them for its whole
  duration. Only one delta command is in flight per group at a time.
- Small deltas fragment device free space; the free list is searched linearly.
- The room under `max_pending_bytes` counts all dead extents, so heavy ordinary overwrites can
  push delta writes back to the previous path until GC catches up.
- While the room is half used, each folding pass runs a GC pass, which scans the whole extent
  table.
- A panic while a delta command is being proposed poisons the group's write lock, as any panic
  under that lock does: the node must be restarted.
- Locality pinning moves base extents only.
- Not done here: coalescing data-node fsyncs, releasing the write lock after proposing on the
  other write paths, preallocating device files.

## What is verified and what is not

Everything below ran on one Linux host in containers, with local file devices and metadata and
data nodes in the test process talking over localhost. Test directories were on tmpfs; the tests
in `delta_writes.rs` and the engine's delta tests were also run on the host's HDD.

Covered by tests:

- the catalog commands: a rejected command changes nothing, replay, reload from the catalog
  store and a streamed Raft snapshot reach the same state, every command that drops an extent map
  drops its deltas, the fold compare-and-swap, batched reclaim, snapshot and truncate refusals
  (`crates/atlas-native/src/metadata/delta/tests.rs`);
- engine internals: term-bound allocation, space kept after an undecided commit, re-reading a
  cell that was folded and reused, replication without the write lock
  (`crates/atlas-native/src/engine/delta/tests.rs`);
- end to end (`crates/atlas-native/tests/delta_writes.rs`): two fixed-seed random runs against
  an in-memory model with reopens; concurrent writers with folds, snapshots and truncates; group
  commit with a stale write and with an injected panic; the capability gate with a node that
  advertises nothing; a leader halted before proposing, after proposing (before and after the
  command commits), followers halted so a proposal cannot commit, and leadership moving away and
  back; replication, repair, GC grace, tiering, erasure coding, deduplication, block-aligned
  devices, the caps, and turning the mode off;
- a FUSE operations-layer client with direct reads on a cell with deltas
  (`crates/atlas-native-fuse/tests/ops.rs`);
- the `atlas-native` and `atlas-native-fuse` tests with the mode off, and again with it on by
  default (`--features atlas-native/delta-writes-test`, which also sets `gc_grace_ms` to 0).
  Three existing tests that count reclaimed extents or direct-read fallbacks fold first, or
  expect the fallback, when the mode is on.

Not verified:

- **performance**: nothing was measured, and none of the defaults are tuned;
- a kernel FUSE mount (`fio --verify`, `git clone` with `fsck`), NFS, SMB, S3 or CSI with the
  mode on;
- a deployed or multi-host cluster, real disks under load, power loss, or killing processes: the
  "leader killed" tests halt a Raft replica inside one test process;
- an older build in the same cluster: the gate and the data-node fallback were not run against
  one (a current node advertising no capability stands in), and no upgrade or downgrade was run;
- the order in which truncate takes its fence and reads the catalog during a failover has no
  test of its own;
- long runs, large catalogs, and behaviour near a full device.

## Measured: one quick run against Ceph (2026-11-28)

`deploy/bench-vs-ceph/compare.sh`, commit `598a6b31`, one shared host, one spinning disk, three
copies for both systems, CephFS (Ceph 20.2.5, ceph-fuse) against atlas-native FUSE. 1 GiB data set,
15 s ramp, **60 s per test, a single run**. IOPS:

| Test | Ceph | atlas-native, mode off | atlas-native, delta writes |
|---|---:|---:|---:|
| 4 KiB random write, queue depth 32 | 38.7 | 3.8 | 56.9 |
| 64 KiB random write, queue depth 32 | 50.1 | 3.8 | 47.2 |
| 4 KiB mixed 70/30, queue depth 32 | 100.3 | 12.9 | 83.3 |
| 4 KiB random write, queue depth 1 | 7.8 | 4.2 | 4.0 |
| 64 KiB random write, queue depth 1 | 5.6 | 3.7 | 4.2 |
| 1 MiB sequential write | 4.6 | 4.1 | 3.6 |
| 4 KiB file create | 26.5 | 4.2 | 4.9 |

- Delta writes led Ceph only on the 4 KiB queue-depth-32 test. Ceph led or was level everywhere else
  in this table.
- **This is not steady state.** After the mixed test, 3,830 deltas in 663 cells were still waiting to
  be folded.
- Folding continued into the later tests: 1 MiB sequential reads after the random writes fell to
  92 MiB/s against 376 MiB/s with the mode off, while about 700 MiB was written to the disk during
  that read.
- **Read results are not comparable** and are left out: the data set fits in memory, atlas-native
  data nodes read through the page cache, and its random read rates are beyond what the disk can do.
- Whole-disk write counters (which include other work on the host) suggest delta writes put roughly
  two to three times more on disk per 4 KiB operation than Ceph, before the backlog is folded.
- Ceph's integrity check was interrupted (an oversized first version) and did not run to completion.
  Both atlas-native runs passed a 4 MiB crc32c write and read-back.
