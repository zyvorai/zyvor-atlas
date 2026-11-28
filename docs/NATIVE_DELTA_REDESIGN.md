<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# Delta writes: redesign plan

**Status: proposed on 2026-11-28. Nothing below is approved or implemented.**

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

## Phase 1: fold in parallel

No change to on-disk formats or commands.

- A bounded pool (`fold_concurrency`) folds different cells at once. The fold already commits by
  compare-and-swap without the write lock; check space leases, reclaim, and the interaction with
  snapshot and truncate under concurrency.
- Fold the cells with the most deltas first, keeping an age limit so none starves.
- Move garbage collection off the folding path.
- Try longer election timeouts if phase 0 ties elections to disk stalls.

Risk: more folding takes disk time from foreground writes. Measure at several concurrency levels.

## Phase 2: commit folds in batches

A `FoldCells` command carrying many compare-and-swap folds, so they share one Raft round. A cell
that changed is skipped on every replica identically; the rest apply. Needs the same capability
gate as delta writes, and replay and snapshot tests.

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

## Decisions needed

1. Approve phases 0 to 2. Delta writes stay off by default throughout.
2. Accept or change the target above.
3. Phase 3 later: go or no go, and where checksums live.
