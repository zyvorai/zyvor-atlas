<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# Delta writes: redesign plan

**Status (2026-11-28): phases 0 to 2 approved. Phase 1 (parallel folding, `fold_concurrency`) was
benchmarked and [did not meet the target](#phase-1-results). Phase 2 (batched fold commits,
`fold_batch_cells`) is therefore due and is implemented; no benchmark result of it is recorded
here. The measurements of phase 0 are [here](#phase-0-results). A
[design for phase 3](NATIVE_DELTA_INPLACE.md) advises against building it yet. Phases 3 and 4 are
not approved.**

## The problem

[Delta writes](NATIVE_DELTA_WRITES.md) make small overwrites fast in a short burst, then lose to
Ceph. In single 20-minute runs of 4 KiB random writes at queue depth 32:

| | Ceph | 1 MiB cells | 128 KiB cells |
|---|---:|---:|---:|
| Write IOPS | 37.8 | 23.8 | 39.2 |
| p99 write latency (ms) | 1334 | 3808 | 2022 |
| 1 MiB sequential read afterwards (MiB/s) | 103 | 5.1 | 3.0 |
| Deltas still unfolded at the end | n/a | not recorded | 39,326 of 47,856 |

The 128 KiB write rate depends on folding that had not happened.

## What the code does

Read from the source; the effect of each on the numbers is inference until phase 0 measures it.

1. **Folding is serial.** `delta_maintenance_once` (`engine/delta.rs`) folds one cell at a time on
   one thread per node (`node.rs`, `maintenance`). The configuration allows 8 cells every 200 ms,
   40 a second. The 128 KiB run managed 2,330 folds in about 1,215 s, **1.9 a second**. The same
   thread also runs garbage collection when pending deltas pass half their cap, as they did.
2. **Every fold is expensive.** `fold_cell` reads the base and all its deltas, writes the whole
   cell again to three replicas, each with its own `fdatasync`, and commits one Raft command for
   that one cell.
3. **Falling behind and keeping up both cost.** In that run a fold consumed 3.7 deltas on average,
   so keeping up at 39 writes a second needed about 11 folds a second. Folding sooner means fewer
   deltas per fold and still more folds. Folding later means slower reads. Copy-on-write folding
   costs three cell writes divided by the deltas it consumes.
4. **Reads pay for every delta.** A cell with *k* deltas takes 1 + *k* extent reads, and each
   extent is read whole to check its SHA-256 (`read_placed`). A 4 KiB read of a 1 MiB cell reads
   at least 1 MiB.
5. **A foreground write needs several syncs:** one `fdatasync` per replica, on a file that grows
   with each append, plus a Raft command shared with other writers. The test disk is a single
   7,200 rpm drive (XFS) shared by every replica. How many such writes it can sustain is unknown.
6. **Leadership moved during the 128 KiB run.** The election timeout is 10 to 20 ticks of 200 ms
   (2 to 4 s) and p99 write latency was about 2 s, so disk stalls are a suspect. Not checked.

### A correction

An earlier suggestion was to point at new small pieces and stop rebuilding cells. That would make
the read problem permanent: the data stays scattered. Ceph's BlueStore, as we understand its
deferred writes, logs a small overwrite, acknowledges it, and later writes it into place. Phase 3
is the equivalent. Smaller changes come first, because point 1 may be most of the problem.

## Target

Proposed gate, on the same host, three replicas, Ceph and atlas-native run alternately in one
session, median of three 20-minute runs:

- write IOPS at or above Ceph's;
- **pending deltas not growing over the last 10 minutes**, so no win rests on postponed work;
- p99 write latency no worse than Ceph's;
- sequential read afterwards at least half of Ceph's;
- `fio` crc32c verification passes.

One comparison pair takes about 70 minutes of host time, so three take about 3.5 hours.

```
ROUNDS=3 CASES="ceph atlas-delta" STEADY_ONLY=1 STEADY_SECS=1200 deploy/bench-vs-ceph/compare.sh <tag>
```

`gate.py` (run by `compare.sh`, or by hand on the result sets) prints PASS, FAIL or UNKNOWN; anything
missing, failed or short of three alternating rounds is never a PASS. It reads the target this way:
medians for IOPS, p99 and the sequential read; the backlog and `fio` checks must hold in **every**
atlas round. "Not growing" allows a rise (end against start, or trend, over the last 600 s) of up to
1% of the delta writes taken then, and a backlog within 10% of a configured limit fails.
`ATLAS_DELTA_EXTRA` adds settings to `delta_writes` in the delta cases, and `atlas-delta-128k` is the
128 KiB-cell case.

## Phase 0: measure

No change in engine behaviour.

- Disk ceiling: `fio` on files under `/data/atlas-bench`, three files, 4 KiB writes each followed
  by `fdatasync`, appending and preallocated.
- Metrics for data and WAL sync count and latency, fold time split into read, write and commit, and
  deltas consumed per fold.
- `run.sh` samples node metrics every minute, including the existing
  `atlas_native_raft_elections_total`, so backlog and elections can be read as a time series.
- Repeat the 1 MiB and 128 KiB runs once each.

**Decision point:** if the disk ceiling for three synced files is close to what both systems
already do, no fold design will raise the write rate much. Go to phase 4 or stop.

## Phase 0 results

### Disk ceiling (2026-11-28, one probe session)

`deploy/bench-vs-ceph/disk-probe.sh`. One logical write is 4 KiB written to three files, each write
followed by `fdatasync` (buffered `pwrite`, as `FileDevice` does it). fio 3.33 in a container
limited to 12 GB and 10 CPUs; files on the benchmark disk (WDC WD10EZEX, 7,200 rpm, XFS, kernel
7.0.0-44, `mq-deadline`). 2 rounds of 25 s per case after a 5 s ramp. fio counted 430,839 writes and
430,839 `fdatasync` calls, and an `strace` check before measuring found every write synced on its own
file. The probe held the benchmark lock; the shared host's other services kept running (1-minute load
2.4 at the start). In 20 s samples the disk took 25 writes a second just before the probe and 3 just
after. Raw results: `/data/atlas-bench/results/probe1/disk-probe` on the benchmark host.

fio cannot make one write wait for three parallel ones, so there are two stand-ins: **chain** (each
writer writes and syncs the three files one after another) and **fanout** (the same number of writers
on each file, independently; the slowest file's rate is quoted, an optimistic figure). atlas-native
writes replicas in parallel, which is neither. Neither bounds the other.

Logical writes a second (median of the 2 rounds):

| writers | appending: chain | appending: fanout | in place, random: chain | in place, random: fanout | in place, sequential: chain | in place, sequential: fanout |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 8.3 | 9.6 | 37.6 | 41.2 | 38.2 | 37.3 |
| 8 | 45.1 | 68.6 | 60.9 | 63.0 | 163 | 301 |
| 32 | 76.4 | 115 | 82.5 | 81.4 | 318 | 249 |
| 128 | 143 | 120 | 96.4 | 93.2 | 250 | 221 |

- "Appending": the three files start empty and grow, writers taking interleaved slots at the end.
  "In place": three 2 GiB files written in full beforehand.
- The two rounds were within 10% of each other except appending fanout at 128 writers (143 and 98)
  and at 1 writer (10.2 and 8.9), and sequential chain at 8 (174 and 153).
- At 32 writers, appending, the median `fdatasync` took 126 ms (chain) and 250 ms (fanout).
- The kernel treats the drive's cache as write-through and had sent it no flush commands. A lone
  sequential in-place sync took 6 ms (median, chain), which suggests writes do reach the platter.
  The drive's own cache setting was not read.

### What it means for the decision point

The systems did 38 (Ceph) and 39 (delta writes, 128 KiB cells) writes a second at queue depth 32, so
the row to compare is 32 writers: **76 to 115 appending, 2 to 3 times what either system did.**

- That is not "close", so the disk alone does not send the plan to phase 4 or a stop. Phases 1 and 2
  can go ahead.
- It is **inconclusive** about whether they can win. The probe is only the foreground data writes. A
  real write also needs the metadata log syncs, and folding, GC and reads share the same disk.
- A rough estimate, not a measurement: keeping up in the 128 KiB run needed about 11 folds a second,
  or 33 more synced writes (of 128 KiB, not the 4 KiB probed) on top of 3 x 39 = 117 foreground ones.
  About 150 a second compares with the 230 to 360 synced 4 KiB appends (all files together) the
  probe reached at 32 writers, before metadata syncs and fold reads. The margin may be small.
- For phases 3 and 4: preallocated files helped when writes stayed sequential (249 to 318 logical
  writes a second at 32 writers, 2.2 to 4.2 times appending in the same model). Random in-place writes
  synced one by one (81 to 83) did no better than appending. With one writer, a sync took 42 ms
  appending and 6 ms writing sequentially in place (median, chain). Phase 3 would sync an in-place
  batch once per device, which this probe did not measure.
- Not covered: files as large and old as a long run leaves them, runs longer than 30 s, other write
  sizes, and any other disk or host.

The rest of phase 0 (sync and fold metrics, the repeat 20-minute runs, elections against disk
stalls) has no results here yet.

## Phase 1: fold in parallel

No change to on-disk formats or commands.

- A bounded pool (`fold_concurrency`) folds different cells at once. The fold already commits by
  compare-and-swap without the write lock; check space leases, reclaim, and the interaction with
  snapshot and truncate under concurrency.
- Fold the cells with the most deltas first, keeping an age limit so none starves.
- Move garbage collection off the folding path.
- Try longer election timeouts if phase 0 ties elections to disk stalls.

Risk: more folding takes disk time from foreground writes. Measure at several concurrency levels.

### Phase 1 results

Three alternating rounds against Ceph, `fold_concurrency` 4, 2026-11-28. Details and caveats:
[NATIVE_DELTA_WRITES](NATIVE_DELTA_WRITES.md#measured-parallel-folding-three-alternating-rounds-2026-11-28).

| Check (median of 3) | Ceph | 1 MiB cells | 128 KiB cells |
|---|---:|---:|---:|
| Write IOPS | 38.0 | 23.6 **fail** | 37.9 **fail** |
| p99 write latency (ms) | 1317 | 3137 **fail** | 2089 **fail** |
| Backlog not growing (rounds passing) | n/a | 2 of 3 **fail** | 0 of 3 **fail** |
| Sequential read (MiB/s) | 102 | 250 pass, from memory | 2.4 **fail** |
| `fio` verification (rounds passing) | n/a | 3 of 3 | 3 of 3 |

The gate's sequential read check is not trustworthy as the harness stands: the data set fits in
memory and atlas-native reads through the page cache. The harness needs a larger data set or a cold
cache. `fold_concurrency` values other than 4 have not been measured.

## Phase 2: commit folds in batches

A `FoldCells` command carrying many compare-and-swap folds, so they share one Raft round. A cell
that changed is skipped on every replica identically; the rest apply. Needs the same capability
gate as delta writes, and replay and snapshot tests.

**Implemented, not benchmarked.** See [batched commits](NATIVE_DELTA_WRITES.md#batched-commits):
one committer per metadata group proposes whatever the folders have queued, without waiting for a
batch to fill, at most `fold_batch_cells` (32) at a time. `fold_batch_cells` 1 gives phase 1's one
command per fold, for comparing the two on one build
(`ATLAS_DELTA_EXTRA='"fold_batch_cells": 1'`).
`atlas_native_delta_fold_batch_cells` shows the batch sizes reached. Snapshot folds are not
batched.

**If the gate is met after phase 1 or 2, stop here.**

## Phase 3: apply deltas in place

The actual redesign. It needs its own approval, with phase 1 and 2 numbers.

A fold writes only the changed blocks into the existing base:

1. The leader commits an intent naming the cell and the deltas to apply.
2. Each replica writes those bytes at their place in the base (`BlockStore::write_at` exists) and
   syncs once per device for a whole batch.
3. The leader commits completion: new checksums, deltas removed.

The deltas stay authoritative until step 3, so a crash or leader change repeats step 2, which is
harmless to repeat.

Requirements and limits:

- **Block checksums.** A whole-extent SHA-256 cannot survive partial updates. In the catalog, a
  4-byte checksum per 4 KiB costs about 1 GiB of memory on every metadata node per TiB written
  (256 MiB with 16 KiB blocks). Keeping checksums on the data nodes avoids that but changes the
  data protocol. This choice is open.
- **Aligned deltas only.** A block being overwritten is then covered entirely by a pending delta,
  so readers ignore its base bytes, including a torn write. Other deltas use the copy fold.
- **Copy fold remains** for bases shared with snapshots or clones, deduplicated, compressed,
  erasure-coded or tiered, and for block-aligned devices.
- **Stale replicas.** A replica that misses an apply must never serve the old bytes. That needs a
  generation number per extent and per replica, and repair that understands it. This is the
  hardest part.
- **"Extents are never rewritten" stops being true.** Audit everything that relies on it: scrub
  and repair, replication, export, direct reads in FUSE and ublk, leases, caches and the dedup
  index.
- Format change: capability-gated, with no downgrade once any extent uses it.

Block checksums would also let a 4 KiB read fetch one block instead of a whole cell.

Tests before any benchmark: halt at every step above, fail a replica mid-apply, change leader,
race snapshots, truncates and clones, and compare byte for byte against a model.

## Phase 4: fewer syncs per write

Only if phase 0 shows syncs are the limit. Candidates: preallocated device files, or one journal
per device. Group commit of `fdatasync` was already tried (branch `exp/native-group-commit`) and
was slower in our test, so it is not repeated without phase 0 data explaining why.

## Not covered

Latency of a single write at queue depth 1, small file creation, NVMe, and more than one host.
atlas-native may still not beat Ceph after all of this. Results from one shared host with one
spinning disk say nothing about other hardware.

## Decisions

1. Phases 0 to 2: approved 2026-11-28. Delta writes stay off by default throughout.
2. The target above is the gate used unless it is changed.
3. Open, for later: phase 3 go or no go, and where its checksums live.
