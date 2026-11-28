<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# atlas-native delta writes

Without this mode a partial overwrite holds the metadata group's write lock, reads the whole
extent, writes it again to every replica and commits, one write at a time. With
`metadata.delta_writes.enabled` a small overwrite instead stores only its bytes, as a **delta**
extent listed beside the cell's base extent, and many writers share one metadata commit. A
background **fold** later rewrites the base with its deltas applied. Several cells are folded at
once, and their commits are batched: see [How folding works](#how-folding-works).

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

## How folding works

A node runs one pool of `fold_concurrency` folder threads over all the metadata groups it
replicates. A folder folds only in a group this node leads, one cell at a time, so at most
`fold_concurrency` cells are being rewritten by background folds on a node at once. Between two
folds a busy folder does not wait: there is no per-second bound other than the number of folders
and, with [batched commits](#batched-commits), the room in the queue.

**Which cells are due.** A cell with `fold_after_deltas` deltas, or dirty for `fold_after_ms` as
this node first saw it. Every dirty cell is due while delta writes are turned off, or while
pending plus dead bytes (or the pending delta count) have used half their room.

**Which goes first.** The leader plans from its dirty-cell index and queues up to 256 of the due
cells with the most deltas and up to 256 of those waiting longest. Three picks in four take the
first queue (most deltas, the longer wait first among equals), the fourth takes the cell that has
waited longest, so a cell with one delta is reached however busy the others are: each fourth fold
takes the oldest. That is a bound in folds, not in time: when folding falls behind the writes,
every cell waits longer. A cell's wait starts again when it is picked.

A plan reads every dirty cell, 4096 per hold of the catalog lock (under Raft, the node's lock).
It is made again when the queues run out (at once if cells were left out, else after
`fold_interval_ms`) or are a second old. An idle folder therefore notices a newly due cell within
about `fold_interval_ms`.

**One fold per cell.** Every fold takes a claim on its cell first and keeps it until the fold has
ended (also while it waits to be committed), so two never work on the same cell. This avoids
wasted work and lets a snapshot wait; it is not what keeps data right. That is still the
compare-and-swap: a write that replaces the base meanwhile (a whole-cell write, a rewrite of a
cell at its delta limit, a truncate, an unlink) takes no claim, and the fold that lost is left
out or skipped, giving its space back.

**Snapshots.** A snapshot folds what no one else is folding, then stops new deltas on its target,
takes the write lock, waits for the folds in progress or queued on its cells and folds the rest.
While a target is stopped, the folders take its cells before any other, which shortens the time
the write lock is held. Neither a folder nor a committer ever waits for the write lock or for a
second claim, so this cannot deadlock; no new delta lands on the target, so it ends under
continuous writes. The folds a snapshot (or `fold_all_deltas`) makes itself are committed one
command each, and a truncate rewrites its cell as an ordinary write.

**Failures.** A folder whose fold fails (a data node down or full, a commit that timed out or
lost its leader) pauses for `fold_interval_ms`, doubling with each failure in a row up to 5
seconds. When a batch has a failed fold, all folders leave that group alone for the same kind of
pause. A fold with an unknown outcome keeps its space reserved for 300 s, as before; if it
applies after all, a later fold of the same cell loses the compare-and-swap. A folder that loses
leadership mid-fold ends that fold: data nodes refuse its writes once they have seen a newer
term, and its commit is bound to its term. A panic in a fold is counted as a failure and the
folder or committer goes on (a batch whose commit panicked keeps its space reserved).

**Reclaim** runs on its own thread, so a slow reclaim does not stop folding, nor the reverse.
Every `fold_interval_ms` after the previous pass ends, it runs a GC pass in each led group whose
pending plus dead bytes have used half the room, as long as delta writes are on or deltas are
pending. The grace period applies as before.

**Backpressure.** The folders stop starting folds while dead, unreclaimed bytes alone reach
`max_pending_bytes`: folding more would only take more device space until reclaim catches up.
Folds already running or queued finish, so dead bytes can pass the limit by about one cell and
its deltas per folder and per fold in or waiting for a batch. Snapshots and truncates are not
held back.

**Shutdown** stops the folders from starting folds and waits for those in progress, each bounded
by the data-node and proposal timeouts, and then for the committers to commit what is queued (at
most the batch in progress and one more, each bounded by the proposal timeout).

**Cost.** Each rewrite holds its cell in memory (a small multiple of `extent_bytes`) and uses up
to 8 reader threads, then one writer thread per replica or shard; a queued fold holds only its
extent record and its claim on device space. Besides the folders, each snapshot being taken folds
one cell at a time on its caller's thread. More folders also take more disk time from foreground
writes. How many suit a given disk has not been measured.

### Batched commits

With `fold_batch_cells` above 1 (the default is 32), delta writes on, and every metadata node
advertising support, a folder does not commit its own fold. It hands the rewritten cell to the
group's queue and takes the next cell, while one committer thread per group proposes whatever is
queued as one `FoldCells` command, so those folds share one Raft round.

- **No added wait.** The committer proposes as soon as it is free and anything is queued; it never
  waits for a batch to fill. A batch is therefore what finished while the previous command was
  committing: often one cell when commits are faster than rewrites.
- **Bounded.** A command carries at most `fold_batch_cells` folds. The queue holds as many again,
  and a folder with a rewrite for a full queue waits. So a group has at most twice
  `fold_batch_cells` folds rewritten and not committed, besides one in each folder's hands.
- **Compare-and-swap per cell.** Under the lock the command is proposed under, the leader leaves
  out folds whose cell changed and content written under another term. When the command is
  applied, each fold is checked again, in order, against the catalog alone: one that does not fit
  is skipped, on every replica alike, and the rest apply. Apply reads no configuration.
- **Outcomes.** A batch that times out, loses its leader or fails to be logged keeps the space of
  every fold in it reserved, as a single fold does. Folds that were never proposed give their
  space back.
- **Otherwise single commands.** With delta writes off (folding what is left), with
  `fold_batch_cells` 1, or while a metadata node has not advertised support, every fold is a
  `FoldCell` command of its own, as before.

Whether batching raises the fold rate, or what it does to foreground writes, has **not been
measured**. Folders that no longer wait for commits can also fold faster and so take more disk
time from foreground writes.

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
| `fold_concurrency` | 4 | Folder threads per node, over all its metadata groups (1 to 32). |
| `fold_batch_cells` | 32 | Most folds one command carries, and most queued for the next (1 to 256). 1 turns [batching](#batched-commits) off. |
| `fold_interval_ms` | 200 | How long an idle folder waits before looking again; the pause between early GC passes; the first pause after a failed fold. It does not pace busy folders. |
| `fold_cells_per_pass` | 8 | No longer limits a node. Still accepted; it bounds one `delta_maintenance_once` call of the library (used by tests). |
| `gc_grace_ms` | 10000 | How long an extent stays dead before reclaim. |

The default of 4 folders is a deliberately low, untuned starting point: enough that folds
overlap their syncs and commits, few enough to leave most of a single disk's queue to foreground
writes and to hold only a few cells in memory. `1` folds one cell at a time (still without the
former bound of 40 a second).

When pending plus dead bytes pass half of the room, every dirty cell is due and GC runs early.
The grace period still applies, so under a sustained load faster than folding and reclaim, writes
fall back to the previous path until room returns. The fallback creates more dead data, not less,
so throughput then drops to roughly that of the mode off.

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
- The batched fold command has its own capability bit. A leader proposes `FoldCells` only once
  every node it replicates to has advertised it, and only with delta writes on; a cluster with an
  older delta-capable node keeps using one command per fold. The command adds nothing to the
  catalog, but **a node running a build without it cannot read a log that holds one**: the
  downgrade steps below cover it, since they wait for the log to compact.
- **Do not downgrade, or add a metadata node running an older build, while the log or the catalog
  may hold deltas.** Older builds cannot decode the new commands and silently drop the new
  catalog fields, which would lose acknowledged writes. Before a downgrade: turn the mode off,
  wait for `atlas_native_delta_pending` to reach 0, and let the Raft log compact past the last
  delta command. Atlas does not enforce this; a downgrade has not been tested.
- A data node from before this change cannot say whether its devices pad writes; it gets no
  deltas. The leader asks again after 60 s.

## Metrics

On a metadata node (engine series carry a `group` label when the node runs several groups).
They count what this node did while it led, since it started:

| Metric | Meaning |
|---|---|
| `atlas_native_delta_writes_total` | Writes committed as deltas. |
| `atlas_native_delta_fallbacks_total` | Eligible writes that rewrote their cell instead. |
| `atlas_native_delta_write_seconds{phase}` | Histogram, one observation per delta attempt and phase. `replicate`: placing the bytes on the data nodes. `commit`: from joining the queue to the outcome, so it includes waiting for the previous delta command and for the write lock. |
| `atlas_native_delta_folds_total` | Folds committed. |
| `atlas_native_delta_folded_deltas_total` | Deltas those folds consumed; divided by the folds, deltas per fold. |
| `atlas_native_delta_fold_conflicts_total` | Folds the catalog rejected at commit, normally because the cell changed first. Their reads and writes were wasted. |
| `atlas_native_delta_fold_seconds{phase}` | Histogram, one observation per fold attempt and phase reached. `read`: the base and its deltas. `write`: placing the new base on its replicas. `commit`: the fold command; for a batched fold, from the start of its batch's commit to the outcome (not the wait in the queue). |
| `atlas_native_delta_fold_batch_cells` | Histogram of the folds in each `FoldCells` command that reached the log; `_sum` over `_count` is the mean batch size. Folds committed on their own are not in it. |
| `atlas_native_delta_dirty_cells`, `atlas_native_delta_pending`, `atlas_native_delta_pending_bytes` | Backlog: cells with deltas, deltas not folded, and their device bytes. |
| `atlas_native_dead_bytes` | Device bytes of dead extents not reclaimed yet. |

Folds made for snapshots and truncates count like background ones. Both histograms include
attempts that failed.

Where the syncs and elections show:

| Metric | Node | Meaning |
|---|---|---|
| `atlas_native_data_sync_seconds{node,device}` | data | Histogram of the `fdatasync` calls of a `file` device (appends and overwrites). `aligned` and `io_uring` devices write with `O_DSYNC` and have no series. Fence and high-water-mark files are not included. |
| `atlas_native_raft_wal_sync_seconds{node,group}` | metadata | Histogram of the Raft log's `fdatasync` calls for appended records, leader or follower. Log compaction, catalog checkpoints and the vote file are not included. |
| `atlas_native_raft_elections_total{node,group}` | metadata | Elections started after a successful pre-vote (not new). |

Latency histograms have buckets from 0.1 ms to 30 s; `_count` is the number of calls. All of
these start from zero when a process restarts.

A node also counts `atlas_native_fold_runs_total` (background folds attempted, including those
that lost their compare-and-swap, each fold of a batch counted; before the folders ran in
parallel it counted passes) and `atlas_native_fold_errors_total` (folds that failed). Early reclaim passes are counted in
`atlas_native_gc_runs_total` and `atlas_native_gc_errors_total`.

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
- While the room is half used, a GC pass runs every `fold_interval_ms`, and each scans the whole
  extent table.
- Planning scans the whole dirty-cell index at least once a second while there is a backlog.
- A failed placement can leave appended bytes that no catalog entry refers to (as for any failed
  write). More folders can leave more of them when a data node keeps failing; the pauses limit
  how often.
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
- the fold planner and claims (`crates/atlas-native/src/engine/folder/tests.rs`): the order,
  that a quiet cell is not starved, exclusive claims and their hand-over, release on panic; and
  on an engine: folding with reclaim stuck, the pause on dead space, a stopped target going first;
- the pool at work (`crates/atlas-native/tests/delta_folding.rs`): the concurrency limit, one
  fold per cell, a snapshot waiting for a fold, truncate, rewrite and unlink during a fold,
  writers, snapshots, truncates and reclaim racing four folders over three reopens against a
  model, turning the mode off, reclaim with every folder stuck, writes failing as on a full
  device, a panic in a fold, shutdown with a fold in progress, a leader halted mid-fold whose
  successor rewrites the cells, a leader losing its quorum mid-fold (its folders then stop),
  idle followers, and a single node folding, reclaiming, shutting down with a backlog and
  restarting. The device failures, halts and stalls are injected inside the test process;
- batched commits: a command that applies in part, one that changes nothing and an empty one,
  replayed, reloaded and carried over a snapshot transfer (`metadata/delta/tests.rs`); leaving
  out changed cells and other terms' content, and keeping the space of a batch whose committer
  panicked (`engine/fold_batch/tests.rs`); the queue limit, changes, a snapshot and shutdown
  while folds are queued, a panic in the committer, the capability gate with a node advertising
  only the earlier capability, a leader halted after a rewrite, before proposing, after proposing
  and after the commit, random writes and truncates with pools on every replica across leader
  changes against a model, and a replica restarted behind a compacted log
  (`tests/delta_folding.rs`); a random run against the model with the pool folding
  (`tests/delta_writes.rs`);
- a FUSE operations-layer client with direct reads on a cell with deltas
  (`crates/atlas-native-fuse/tests/ops.rs`);
- the `atlas-native` and `atlas-native-fuse` tests with the mode off, and again with it on by
  default (`--features atlas-native/delta-writes-test`, which also sets `gc_grace_ms` to 0).
  Three existing tests that count reclaimed extents or direct-read fallbacks fold first, or
  expect the fallback, when the mode is on.

Not verified:

- **performance** beyond the two single runs below; none of the defaults are tuned. Both runs
  predate parallel folding and batched commits, **neither of which has been benchmarked**:
  neither the fold rate, the batch sizes reached, the effect on foreground writes, nor any
  `fold_concurrency` or `fold_batch_cells` other than by the tests' correctness checks;
- more than one metadata group per node with the folders under load;
- a data node that is really out of space (the test fails its writes instead);
- a kernel FUSE mount with the mode on beyond the benchmark runs below and their 4 MiB `fio`
  verify (no `git clone` with `fsck`); NFS, SMB, S3 or CSI with the mode on;
- a deployed or multi-host cluster, real disks under load, power loss, or killing processes: the
  "leader killed" tests halt a Raft replica inside one test process;
- an older build in the same cluster: the gate and the data-node fallback were not run against
  one (a current node advertising no capability stands in), and no upgrade or downgrade was run;
- the order in which truncate takes its fence and reads the catalog during a failover has no
  test of its own;
- runs longer than 20 minutes, large catalogs, and behaviour near a full device.

## Measured: one quick run against Ceph (2026-11-28)

The runs below were made with the earlier, serial folding, one command per fold.

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

## Measured: 20 minutes of 4 KiB random writes (2026-11-28)

`PROFILE=quick STEADY_ONLY=1 STEADY_SECS=1200 CASES="ceph atlas-delta"`, commit `f3b3e359`, same
host and 1 GiB data set, queue depth 32, a single run.

| | Ceph | atlas-native, delta writes |
|---|---:|---:|
| Mean write IOPS over 1200 s | 37.8 | 23.8 |
| p50 / p99 write latency | 868 / 1334 ms | 994 / 3808 ms |
| MiB written to the disk during the test | 2,836 | 4,080 |
| 1 MiB sequential read afterwards | 103 MiB/s | 5.1 MiB/s |
| 4 KiB random read afterwards, queue depth 32 | 145 IOPS | 797 IOPS |
| p99 of that random read | 434 ms | 1019 ms |
| 4 MiB crc32c write and read-back | passed | passed |

- **The short-run lead does not last.** Over 20 minutes Ceph completed about 1.6 times as many
  writes as delta writes, with a much shorter latency tail.
- Delta writes slowed sharply after roughly the second minute. The time log of this run is too
  coarse (a whole number per job) to give a curve; `run.sh` now logs every completion instead.
- The likely reason is that folding competes with new writes for the one disk. 348 MiB was still
  being written during the 60 s sequential read afterwards. Node metrics were not captured, so
  backlog, fold counts and fallbacks for this run are unknown.
- The random read figures are partly served from memory and are not a fair comparison.
- Today's atlas-native (mode off) was not run for 20 minutes. In the short run it managed 3.8 IOPS.
- The host is shared; load was high during both runs (higher at the end of the delta run).

**Conclusion:** on this host, atlas-native with delta writes does not beat Ceph at sustained small
random writes. It is not ready to be enabled by default.

## Measured: the same 20 minutes with 128 KiB cells (2026-11-28)

`ATLAS_EXTENT_BYTES=131072`, delta writes on, commit `d9b9d843`, a single run in a later session
than the Ceph run it is set against (the host was quieter when this one started).

| | Ceph | delta writes, 1 MiB cells | delta writes, 128 KiB cells |
|---|---:|---:|---:|
| Mean write IOPS over 1200 s | 37.8 | 23.8 | 39.2 |
| p50 / p99 write latency (ms) | 868 / 1334 | 994 / 3808 | 726 / 2022 |
| 1 MiB sequential read afterwards (MiB/s) | 103 | 5.1 | 3.0 |
| 4 KiB random read afterwards, queue depth 32 (IOPS) | 145 | 797 | 52 |
| Sequential layout write (MiB/s) | 6.1 | 4.7 | 2.9 |

- Write IOPS stayed between 37 and 44 per minute for 17 minutes, then 25 to 34 in the last three.
- **The write rate rests on postponed work.** Summed over the metadata nodes, 47,856 delta writes
  were taken and 2,330 folds had run when the test ended; 39,326 deltas (82%) in 7,791 cells were
  still waiting. No write fell back to the old path.
- `max_pending_deltas` is 65,536. At that rate it would be reached about 11 minutes later, after
  which writes go back to rewriting the cell. That was not run.
- Reads afterwards were far behind Ceph's.
- More than one metadata node counted delta writes, so leadership changed during the run. The cause
  was not investigated.
- Disk write totals are not compared: most of this run's folding had not happened yet.

Smaller cells do not make delta writes beat Ceph here. A change that folds more cheaply, or does
not need to rewrite whole cells, would be needed.

What to change next is proposed in [NATIVE_DELTA_REDESIGN](NATIVE_DELTA_REDESIGN.md).

## Measured: parallel folding, three alternating rounds (2026-11-28)

Commit `865eee02`, `fold_concurrency` 4. Ceph, 1 MiB cells and 128 KiB cells in turn, three rounds
of each, 20 minutes of 4 KiB random writes at queue depth 32. One shared host, three replicas on
one 7,200 rpm disk. Medians of the three rounds, with the range in brackets. Result sets:
`/data/atlas-bench/results/gate1-*` on the benchmark host.

| | Ceph | 1 MiB cells | 128 KiB cells |
|---|---:|---:|---:|
| Write IOPS | 38.0 (37.8 to 42.4) | 23.6 (22.4 to 26.3) | 37.9 (36.1 to 41.9) |
| p99 write latency (ms) | 1317 (1267 to 1351) | 3137 (2668 to 3305) | 2089 (2055 to 2164) |
| Longest write (s) | 1.7 to 2.8 | 4.1 to 4.9 | 10.6 to 12.1 |
| Disk MiB written during the long run | 2854 | 14444 | 4742 |
| Deltas unfolded at the end | n/a | 7 to 10% | 35 to 38% |
| Folds a second, last 10 minutes | n/a | 3.5 to 4.5 | about 7 |
| Raft elections during the long run | n/a | 0, 0, 0 | 5, 3, 6 |
| 1 MiB sequential read afterwards (MiB/s), see below | 102 | 250 (125 to 258) | 2.4 (2.1 to 2.7) |
| 4 MiB crc32c write and read-back | passed | passed | passed |

**Neither atlas-native case meets the [target](NATIVE_DELTA_REDESIGN.md#target).**

- 1 MiB cells fail write IOPS and p99 latency. The pending-delta backlog was judged not growing in
  two rounds and growing in the third (a trend of +244 over 10 minutes against 144 tolerated).
- 128 KiB cells fail write IOPS (37.9 against 38.0, level within the spread between rounds), p99
  latency, the backlog in all three rounds (it grew by 5,000 to 6,000 deltas in the last 10
  minutes) and sequential read.
- No run recorded a failed workload, a delta fallback or a rejected fold.
- **The sequential read figures are not a fair comparison, including the one the gate passed.** The
  data set is 32 files of 32 MiB and atlas-native's file devices read through the page cache. With
  1 MiB cells the disk supplied about 1.1 GiB during a job in which `fio` read 7.3 to 15 GiB, so
  most of it came from memory. Ceph read about 4.7 GiB from disk while `fio` read about 6 GiB.
  A larger data set, or reading with a cold cache, is needed before any read claim.
- Host load during the long run was higher with atlas-native (median 1-minute load 32 to 40) than
  with Ceph (about 22). The load includes the system under test.
- Compared with the earlier single runs with serial folding: write IOPS are about the same, the
  128 KiB backlog fell from 82% to about 37%, and 1 MiB p99 fell from 3808 to a median of 3137 ms.
  Those were different sessions with one run each.

Means over the long run (range of the three rounds):

| Time (ms) | 1 MiB cells | 128 KiB cells |
|---|---:|---:|
| Data device sync | 141 to 157 | 143 to 157 |
| Raft log sync | 125 to 135 | 94 to 113 |
| Delta write: replicate the data | 554 to 874 | 398 to 437 |
| Delta write: commit | 307 to 429 | 265 to 308 |
| Fold: read | 213 to 355 | 51 to 64 |
| Fold: write | 359 to 466 | 265 to 275 |
| Fold: commit | 224 to 330 | 186 to 228 |
| Deltas consumed per fold | 5.6 to 5.8 | 3.3 to 3.6 |

*Inferred:* with 128 KiB cells a fold took about 0.5 to 0.55 s and four ran at once, which is the
observed 7 folds a second. At about 3.5 deltas a fold, keeping up with 37 writes a second needs
about 11. Whether batching commits closes that gap depends on folder threads not waiting for them;
more folder threads would also take more disk time from writes. Neither has been measured.

The elections and the stalls of over 10 seconds both appeared only with 128 KiB cells. No cause has
been established.

