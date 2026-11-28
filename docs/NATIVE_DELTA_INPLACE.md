<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# Delta writes phase 3: applying deltas in place

**Status: design only. Nothing here is implemented or measured.** It expands phase 3 of
[NATIVE_DELTA_REDESIGN](NATIVE_DELTA_REDESIGN.md) and builds on
[NATIVE_DELTA_WRITES](NATIVE_DELTA_WRITES.md). File and line numbers are at commit `865eee02`.
Phase 2 (`FoldCells`) was not in that commit; its command is assumed to skip, identically on every
replica, a cell that changed.

## Verdict

**Do not build phase 3 yet, and do not build it as the redesign plan sketches it.**

1. **It is unlikely to raise the small random write rate on the benchmark disk.** It changes only
   folding. In the 128 KiB run folding barely happened (1.9 folds a second, 82% of deltas left) and
   atlas-native still did only 39 writes a second against Ceph's 38. On that evidence (one run, in
   which leadership also moved) the foreground path alone is already near the target: no fold
   design adds writes to it, and a fold that keeps up takes disk time from it.
2. **On a spinning disk it can cost more positioning than copy folding.** In place writes each
   dirty range to three replicas. A copy fold writes the cell three times however many deltas it
   consumes. For 128 KiB cells with about 4 scattered deltas per fold (the run measured 3.7), the
   model below favours a batched copy fold by a wide margin.
3. **It removes the invariant that a live extent never changes**, which scrub, repair, tiering,
   locality pinning, reclaim, dedup, export and direct reads rely on. Several of these would lose
   acknowledged data if left unchanged (marked **loss** in the [audit](#audit)).
4. **The sketch has holes**: a replica that misses the apply, an apply write that arrives after
   the range was reused, readers verifying old checksums, and tier or repair committing a copy
   read before the apply.

What it can win: sequential reads after random writes (bases stay where they were laid out), 4 KiB
reads of large cells (block checksums), and far fewer bytes written per fold on large cells. Those
are real reasons to build it, mainly for 1 MiB and 4 MiB cells or flash. They are not yet shown to
be worth the complexity.

Recommended order: run the [experiments](#experiments-before-any-code) (hours), build the
[batched copy fold](#a-simpler-design-first) (small, no format change), measure, then decide. If
phase 3 is built, build the design below.

## What can be gained

### Writes

Measured (see the other two documents; single runs, one shared host, one 7,200 rpm disk): delta
writes 23.8 (1 MiB cells) and 39.2 (128 KiB cells) writes a second, Ceph 37.8; the disk probe
reached 76 to 115 synced three-copy appends a second at 32 writers, without metadata syncs.

In place changes nothing about a foreground write: three data appends with `fdatasync` and a Raft
commit.

**Model, not measurement.** Disk time per fold, with assumed positioning cost p = 10 ms,
transfer B = 150 MB/s, 4 ms per sorted 4 KiB write in a large synced batch, 4 deltas per fold, no
two adjacent. None of these constants was measured on this disk.

| Fold | 128 KiB cells | 1 MiB | 4 MiB |
|---|---:|---:|---:|
| Copy, one cell per sync (today, and phase 2) | 44 ms | 68 ms | 152 ms |
| Copy, batched (64 / 16 / 4 cells per write and sync), base read from disk | 14 ms | 40 ms | 129 ms |
| The same, base read from page cache | 3 ms | 23 ms | 91 ms |
| In place, 4 deltas | 48 ms | 48 ms | 48 ms |
| In place, 1 delta | 12 ms | 12 ms | 12 ms |

In place also reads the whole base once, the first time an extent is folded this way.

Expectation (inference): no gain over a batched copy fold at 128 KiB; unclear at 1 MiB; a gain at
4 MiB, or where a cell collects few distinct ranges. Overwrites of the same block merge before a
fold in both designs.

### Bytes written per fold (arithmetic)

4 dirty 4 KiB blocks: copy writes 3 x cell, in place 3 x 16 KiB, so 8, 64 and 256 times fewer fold
bytes at 128 KiB, 1 MiB and 4 MiB. This matters for flash wear and bandwidth-bound disks. No NVMe
result exists.

### Reads

- **Sequential read after random writes.** A copy fold moves the cell, so a file's cells end up
  scattered in fold order. In place keeps them where they were first written. This is the clearest
  advantage. It appears only once the backlog is folded: the 5.1 and 3.0 MiB/s measured were
  mostly unfolded deltas, which no fold method fixes unless folding keeps up.
- **4 KiB reads.** With block checksums a read can fetch one block instead of a whole cell (last
  commit of the plan; not automatic). With the constants above that is about 8% faster at 128 KiB,
  1.7 times at 1 MiB and 3.8 times at 4 MiB for an uncached read, and saves hashing the whole cell
  when cached.

## Experiments before any code

All on the test disk, under `/data/atlas-bench`, scheduled one at a time. **The commands were not
run**; check fio syntax on a short run first. Use the limits of `disk-probe.sh`.

**E0. Write ceiling without folding** (no code; two 20-minute runs). Push folding out of reach:

```
EXTRA='"fold_after_deltas": 1000000, "fold_after_ms": 86400000, "max_deltas_per_cell": 1000000'
EXTRA="$EXTRA"', "max_pending_deltas": 1000000, "max_pending_bytes": 17179869184'
ATLAS_DELTA_EXTRA="$EXTRA" CASES="atlas-delta atlas-delta-128k" STEADY_ONLY=1 STEADY_SECS=1200 \
  deploy/bench-vs-ceph/compare.sh <tag>
```

Confirm from metrics that folds stayed at zero. If the rate is below about 1.2 times Ceph's (45),
**no fold design meets the write target with useful margin**; phase 3 can then only be justified by
reads or bytes written.

**E1. Fold cost, in place against batched copy.** Three files per stream, one per replica. `fg` is
the foreground (appends, each synced, 11 writers a file). The fold streams are paced to keep up with
40 writes a second. S is the cell size in bytes, k the dirty blocks per fold, N the cells per batch
(64, 16 and 4 for the three sizes).

```
F=/data/atlas-bench/probe3     # a b c: 2 GiB each, written in full beforehand; fg-a fg-b fg-c: empty
common="--ioengine=psync --direct=0 --fallocate=none --time_based --runtime=120 --ramp_time=10"
fg=$(for f in a b c; do echo --name=fg-$f --new_group --filename=$F/fg-$f --numjobs=11 \
  --rw=write:40k --offset_increment=4k --size=2g --bs=4k --fdatasync=1; done)
# C (in place): 40 random 4 KiB writes a second per file, one sync per 64. Independent of S and k.
fio $common $fg $(for f in a b c; do echo --name=ip-$f --new_group --filename=$F/$f \
  --rw=randwrite --bs=4k --fdatasync=64 --rate_iops=40; done)
# B (batched copy): 40/k cells a second, N cells per write and sync, plus the base reads
fio $common $fg $(for f in a b c; do echo --name=cp-$f --new_group --filename=$F/$f \
  --rw=write --bs=$((N*S)) --fdatasync=1 --rate=$((40*S/k)); done) \
  --name=base --new_group --filename=$F/a --rw=randread --bs=$S --rate=$((40*S/k))
```

Run S = 128 KiB, 1 MiB, 4 MiB and k = 1, 4, 16 for B, once as written and once with `--direct=1` on
`base` (uncached reads). Report foreground writes a second (the slowest of the three files) and
whether each fold stream held its rate. C leaves out the one-time base read per extent.

**E2. Cost of scattered cells** (minutes). The first command reads a file as laid out; the second
reads the same cells in a random order, which is a logical sequential read after every cell moved.

```
fio --name=laid --filename=$F/a --size=1g --rw=read     --bs=1m --ioengine=libaio --iodepth=8 --direct=1
for S in 128k 1m 4m; do
  fio --name=moved --filename=$F/a --size=1g --rw=randread --bs=$S --ioengine=libaio --iodepth=8 --direct=1
done
```

It is optimistic for copy folding (cells stay within 1 GiB).

**Go only if**, at the cell size that will ship and the deltas per fold observed there:

- E1: foreground writes with C are at least 1.25 times those with B; **or**
- E2: `moved` is below the read target (half of Ceph's 103 MiB/s, so 51) **and** E1's C is no more
  than 10% below B **and** the batched fold's ordering (next section) does not close the gap.

Otherwise do not build phase 3 for this hardware. A flash result would need its own run.

## A simpler design first

**Batched copy fold.** On top of `FoldCells`:

1. Claim up to N due cells, ordered by (file, offset); read and verify each.
2. Per target (node, device), concatenate the new cells and place them with **one** `write_at` into
   one free run, or one `append`. Both exist: no new data node request.
3. Each extent gets its own offset inside that run; commit one `FoldCells`.

One sync per device per batch instead of one per cell, one sequential write, and cells folded
together land next to each other in (file, offset) order. No format change, no capability gate, and
extents stay immutable. A failed node moves its whole share to a spare. Bound the batch by bytes
(it is held in memory; a request is at most 256 MiB). It does not reduce bytes written and does not
restore a file's original layout.

Two more options that keep immutability, if reads are the goal: block checksum tables written once
with each new extent (partial reads, at the catalog cost below), and picking logically adjacent
cells in the fold planner.

## Design

### Invariants

- **I1.** A finished revision of an extent is on **every** replica the extent lists. An apply that
  any replica did not confirm is never finished.
- **I2.** While an apply is pending, base blocks covered by its deltas are undefined on every
  replica; all other blocks still hold the old revision.
- **I3.** Only an extent with exactly one reference is changed. So two snapshots that exist at the
  same time and name the same extent id see the same bytes.
- **I4.** An extent gets at most one apply at a time, and an abandoned apply is never retried: the
  extent is replaced and dies.
- **I5.** Space of an extent with an unconfirmed apply is not reused until the data node has
  guaranteed that no write of that apply can still run.
- **I6.** An apply never changes logical content.

### The copy fold is the only recovery path

The sketch repeats step 2 after a failure. This design does not. Any failure, timeout, leader
change or restart leaves a pending apply, and the leader **copy folds** that cell: it reads the
base blocks no delta covers (unchanged, by I2), lays the deltas over them, writes a new extent and
commits the ordinary fold, which replaces the old base. This removes per-replica generations, a
stale replica state, a redo path and repair of half-applied extents. A replica that missed an apply
is never listed in a finished extent, so it cannot be served.

### Which cells

A cell is applied in place only if all of these hold; the catalog checks every one it can see.
Otherwise it is copy folded.

- delta writes and `in_place.enabled`; capability gate below; no pending apply on the base;
- base: one reference, not a delta, replicated (no `ec`), not tiered, not compressed;
- every base replica on a node that is up, byte granular (`file` backend) and answers the new
  feature request; **every replica offset a multiple of 4096**;
- every delta consumed starts and ends on a multiple of 4096, and every block touched lies wholly
  inside the base (a partial tail block is never written in place);
- the scope is not blocked by a snapshot (snapshots always copy fold);
- policy: cell at least `min_cell_bytes`, at most `max_ranges` contiguous dirty ranges (set both
  from E1), table and pinned-space caps not exceeded.

Offsets: `FileDevice::append` writes at the current end, so one append of an odd length (a partial
tail cell, a small compressed extent) leaves later cells unaligned. An unaligned block would share a
disk sector with a neighbour, possibly another extent, and a torn in-place write could damage it.
Hence the alignment rule. Export the fraction of eligible cells; if it is low, align appends of
4 KiB or more (and prefer aligned free ranges) in a separate change.

### Block checksums: in the catalog, 4 KiB, only where needed

**Block size is 4 KiB.** The rule that makes torn blocks harmless is that a pending delta covers
each block being written. 4 KiB random writes do not cover a 16 KiB block; using one would first
need a replicated image of the merged block, which is a small copy fold plus the apply.

Cost of a CRC32C table, per TiB of data that has a table:

| Block | Raw bytes | Per 128 KiB / 1 MiB / 4 MiB extent |
|---|---:|---|
| 4 KiB | 1 GiB | 128 B / 1 KiB / 4 KiB |
| 16 KiB | 256 MiB | 32 B / 256 B / 1 KiB |
| 64 KiB | 64 MiB | 8 B / 64 B / 256 B |

For comparison, an extent record is very roughly 0.5 KiB in memory (estimated from the structs, not
measured): about 4 GiB per TiB at 128 KiB cells, 0.5 GiB at 1 MiB, 0.13 GiB at 4 MiB. A 4 KiB table
adds 1 GiB to that: a quarter, twice and eight times as much again. Every metadata node holds it; a
leader also keeps
a speculative clone of the catalog (`raft.rs:1005`) and clones it to send snapshots
(`raft.rs:1213`), so the table must be an immutable shared allocation (`Arc<[u8]>`, replaced, never
edited), or the leader pays twice.

| Option | Memory | Why not, or why |
|---|---|---|
| **Catalog (chosen)** | above, capped | One authority, replicated and crash safe already. Detects stale, torn and misplaced blocks end to end. No new data node state |
| Data node keeps checksums | none | Needs its own crash-consistent store updated atomically with data, extra syncs, and a generation per replica to catch a replica that is consistent but old. Checksum and data fail together |
| Table on data node, its digest in the catalog | 32 B per extent (the field exists) | Table needs its own copy-on-write range, repair and rebuild; a cold read costs one more seek, which removes the read gain on this disk. Fallback if measured memory is too high |

A table is created the **first** time an extent is applied in place: the leader reads the base,
verifies its SHA-256 and computes the CRCs. Extents never applied cost nothing.
`in_place.max_table_bytes` per group (start at 256 MiB, which covers 256 GiB) stops new tables; an
extent past the cap is copy folded. CRC32C detects bit rot and old data, not an adversary; a wrong
block passes with probability about 2^-32. `crc-fast` is already in `Cargo.lock` (check that it
offers CRC32C and its license before adding it directly).

### Catalog format

```rust
// ExtentRef
rev: u64,                      // content revision, starts 0, only ever grows; omitted when 0
blocks: Option<BlockSums>,     // { block_bytes: u32, crc: Arc<[u8]> /* base64 */ }
// When `blocks` is set, `checksum` is zero and unused. `checksum` becomes private behind
// `verify(payload, covered)` and `content_sha() -> Option<[u8; 32]>`, so the compiler lists users.

// ExtentMeta
apply: Option<Apply>,          // { seq: u64, consumed: Vec<ExtentId>, new_crcs: Vec<(u32, u32)>,
                               //   unsealed: Vec<ReplicaRef> }
modified_ms: u64,
```

`rev` must be permanent. A revision that went back to its old value would let a tier or repair
command prepared earlier apply to different bytes.

No new top-level `Catalog` field, so `store.rs` `State` and the snapshot header
(`raft_snapshot.rs:77-101`) do not change. The set of pending applies and the table and pinned byte
totals are derived on load, like `fill_delta_totals`.

### Commands

All check everything before changing anything and read only the catalog. Batch items that no longer
match are skipped identically everywhere.

| Command | Item | Effect when it matches |
|---|---|---|
| `InPlaceBegin` | cell, base, `rev`, consumed delta ids, table (first time), new CRCs | Match: base is the cell's, eligible as far as the catalog knows, same `rev`, no apply, consumed is a prefix of the delta list, aligned. Installs the table if new; sets `apply` with `seq` = this entry's index and `unsealed` = all replicas. Deltas stay listed |
| `InPlaceFinish` | extent id, `seq`, time | Match: `apply.seq` equals. Writes the new CRCs, `rev += 1`, clears `apply`. If the extent is still the cell's base: removes the consumed deltas and updates the dirty index. If it died meanwhile: nothing else (every replica confirmed, so it is reclaimable) |
| `ReleasePinned` | extent id, `seq`, sealed replicas | Dead extents only: frees those replicas' ranges and removes them from the extent and from `unsealed`; when none is left clears `apply`, so the record can be reclaimed |
| `SealExtent` | extent id, `rev`, SHA-256 | No apply pending: replaces the table with a whole checksum, `rev += 1`. For tiering and downgrade |

Changed existing rules:

- `ReplaceReplica` and `TierExtent` carry `rev` (default 0) and are rejected on a mismatch or a
  pending apply.
- `check_shared` rejects an extent with a table or an apply.
- `check_reclaim` rejects an extent with an apply, and `gc::collect_candidates` leaves it out
  (`ReclaimExtents` is all or none, so one such id would stop the whole batch).
- `FoldCell`/`FoldCells` are unchanged. Replacing the base is how an apply is abandoned. Every
  command that drops a base keeps `apply` on the dead record.

### Protocol

One batch holds claims (`folder.rs`) on all its cells, taken with `try_claim` only, so it never
waits for a claim or the write lock. All steps use the fence from `term_fence`. Bound a batch by
cells and bytes: a snapshot may wait for it while holding the write lock.

| Step | Action | If it fails or the leader stops here |
|---|---|---|
| 1 Prepare | From one catalog view per cell: read and verify the deltas; first time, also the base. Merge deltas in list order into final blocks; compute their CRCs | Nothing changed |
| 2 Begin | `commit(InPlaceBegin, Some(fence))`, **wait until applied**, then keep the items whose `apply.seq` and consumed list are ours | Unknown outcome: apply nothing. If it commits later it is an orphan, step 5 |
| 3 Apply | `check_log()`. One `Apply` request per (node, device): all that device's blocks, sorted by offset; the node syncs once. Join every request thread | Any missing confirmation abandons every item with a replica there (step 5) |
| 4 Finish | Items whose replicas all confirmed: `commit(InPlaceFinish, Some(fence))`. Consumed deltas die and are reclaimed after `gc_grace_ms` | Unknown: it either applied (done) or the item is an orphan, step 5 |
| 5 Abandon | A live extent with `apply` that no batch of this leader is working on is folded first, by copy fold | Retried like any fold. A fold prepared before a late `InPlaceFinish` loses its compare-and-swap |
| 6 Release | For each extent dead for at least `gc_grace_ms`, with `apply`, that no running batch holds: send `Seal` to each unsealed replica's node; commit `ReleasePinned` for those that answered | Node down: its range stays pinned until it answers |

Abandonment backs off in-place folding for that group (doubling, as folder failures do), and a node
that failed is marked down, which makes its cells ineligible.

There is no third state to repair: after any interruption an extent is untouched, finished, or
pending, and pending always ends in step 5.

| Event | Result |
|---|---|
| Replica fails or times out mid-apply | No finish. Copy fold writes full redundancy elsewhere; its old range pinned until sealed. Without enough healthy nodes the fold waits, as any write would; reads are served, and snapshots of that scope fail as they do today |
| Data node restarts mid-apply | Same; the restart ends its writes, and a later `Seal` releases the range |
| Leader dies at any step | New leader: after `leader_ready`, every pending apply is an orphan, steps 5 and 6 |
| Old leader still running | Its commits are bound to its term and rejected. Its writes touch only covered blocks of an extent that is abandoned. `Seal` carries the new term and blocks them before reuse |
| Metadata quorum lost | Step 2 or 4 unknown; reads still lay deltas over the base |
| Power loss everywhere | A committed Begin is durable before any block is touched; deltas are durable; torn blocks are covered; copy fold |
| Local engine: log sync fails | Begin returns an error, nothing is applied; `check_log` blocks later applies until reopen |
| Cell overwritten, cut or removed mid-apply | Base dies with `apply`. If all confirmed, Finish releases it; otherwise step 6 |
| Snapshot requested mid-apply | Waits for the claim; a pending apply left over is copy folded. `CreateSnapshot` is refused while deltas remain, as today |
| Writes during an apply | New deltas append on top and are overlaid; they are not part of this batch |

### Data node protocol

New requests; an older node drops the connection, so `Features` is asked first and cached as
`Granularity` is.

```
Features { device }                       -> Features { apply: bool }
Apply { fence, group, device, seq, parts: [[offset, len]], len, grant? } + payload -> Applied
Seal  { fence, group, through }           -> Sealed
```

- `Apply`: at most 1024 parts (the header limit is 64 KiB), each 4096-aligned, non-overlapping,
  inside the device, lengths summing to `len`. Passes `fenced()` (term), then requires
  `seq >= floor` of the group. Writes every part, then **one** `fdatasync` through the
  `SyncGuard`. `Applied` means all parts are durable.
- `Seal`: passes `fenced()` (which durably raises the term), waits for running applies of that
  group (they hold a lock shared, `Seal` takes it exclusively), sets
  `floor = max(floor, through + 1)`. It writes no data, so a device that failed still answers.
- The floor is in memory only. A restart ends every running write, and old requests cannot arrive
  on dead connections; a deposed leader reconnecting is stopped by the durable term fence that
  `Seal` raised.
- A running apply also holds the term fence shared, so a new leader's first write to that node
  waits for it. This is another reason to bound batches.
- A batch is sized so that one `Apply` per device carries it.
- Concurrent batches are allowed. Sealing a newer batch first can refuse an older one, which then
  abandons; that wastes work only.
- The client sends `Apply` **once**, on a fresh connection, never through the retry of
  `exchange_pooled` (`data_node.rs:1492-1501`): a retried request could succeed while the first
  copy is still running.
- With a grant key, the engine mints a new grant form covering node, group, fence, device, `seq`,
  length and a SHA-256 of the part list. Clients are never leased one.
- Device layer: `BlockStore::apply(fence, parts)`, default "unsupported"; `FileDevice` implements
  it. `aligned` and `io_uring` devices are excluded (they sync per write).

Why `Seal` is needed: today a late write can only hit a range its own placement leased. After an
abandoned apply the range would be freed and given to a new extent, and a stalled apply could then
write over it on several replicas. The term fence does not stop a write of the same term. (A late
ordinary `write_at` after a failed placement is a similar, existing exposure, since
`inflight::Lease` releases the range on failure; this design does not fix it.)

### Reads

- Verify a base with a table **block by block**, skipping blocks wholly covered by a delta in the
  same view; the overlay replaces them. Try each replica until one passes. Without a table, verify
  the whole SHA-256 as now.
- A view is then always a consistent state: an unchanged block passes; a block changed since the
  view fails its old CRC unless the view's deltas hide it. A reader returns the view's content or
  an error, never a mix.
- On a mismatch, look the cell up again (`read_view`). If it looks unchanged, **run a read barrier
  and look once more** before reporting corruption: a follower's catalog may not hold the `Begin`
  that the data nodes already reflect. `Begin` is committed before any write, so the barrier shows
  it.
- `?stale=1` reads (`node.rs:1207`, `node/fs_api.rs:40`) can now fail with a retryable error where
  today they return old data. Document it.
- `file_layout` lists no replica for an extent with a table, as it already does for cells with
  deltas. Old and new direct readers go through the leader. This loses direct reads for such data
  until clients learn block tables (a later change).
- Count verification retries separately from checksum failures.

### Torn writes

Parts are whole 4 KiB blocks at 4 KiB device offsets, covered by durable replicated deltas until
every replica confirmed `fdatasync`. A torn or partly persisted apply therefore only affects blocks
nothing reads from the base, and the extent is replaced. The assumption is that a torn write
damages nothing outside the blocks written; it relies on the filesystem and partition being 4 KiB
aligned and on `fdatasync` flushing the drive. A copy-on-write filesystem below does no harm. After
`Finish`, damage means the device lied; the block CRC detects it and scrub replaces the replica.

### Fail-stop sync handling

`SyncGuard::run` covers writes the calling thread made before the call, so one sync per apply is
within its contract, and an error reported to any concurrent sync fails the apply. A failed device
refuses further writes: it cannot confirm, its cells are abandoned and copy folded elsewhere.

The comment at `device.rs:103-106` ("ranges acknowledged before the failure are unaffected") needs
a qualification: after a failed apply, the *covered* blocks of an acknowledged extent may hold new,
old or evictable page cache contents. Nothing reads them (I2), and after a reopen they are still
never read, because the apply is still pending or the extent dead. A write error part way through
the parts (not a sync error) does not fail the device, as today.

### Interactions

- **Scrub, repair, rebuild, pin:** skip an extent with a pending apply; verify tabled extents block
  by block; re-check `rev` before counting a replica bad or an extent unrecoverable. A lost node's
  pending applies are abandoned, after which the extents are ordinary. Leave dead pinned extents
  out of degraded counts.
- **Erasure coding, compression, tiering:** such extents are never applied. Tiering a tabled extent
  reads it, then commits `SealExtent` and `TierExtent` with the new `rev`. Coldness uses the later
  of `created_ms` and `modified_ms`.
- **Dedup:** a shared extent has several references and is never applied; a tabled one is never
  offered for sharing.
- **Snapshots, clones:** unchanged. Their extents have two or more references. Deltas on a shared
  base are copy folded.
- **Replication:** `diff_inode` compares extent ids (`engine/replica.rs:156`); I3 keeps that
  correct. Replica filesystems take no deltas.
- **Export:** a tabled extent has no content hash before it is read: read, hash, then decide
  whether the blob exists. Reuse still saves the upload, no longer the read.
- **Local engine:** same steps; `Seal` is a no-op because device calls are synchronous.
- **Unchanged:** cache leases, read leases, file locks, FUSE attribute, name and read-ahead caches
  (all keyed by inode over logical content), ublk (it reads through the node API,
  `atlas-native-ublk/src/volume.rs:9-14`; a future direct read there must follow the `file_layout`
  rule), S3, CSI, NFS, SMB, direct writes (they use free ranges only), activity windows. A search
  found nothing extent-aware in `cufile.rs`, `gpu.rs` or the RDMA modules.

### Caps, configuration, metrics

`metadata.delta_writes.in_place`: `enabled` (false), `min_cell_bytes`, `max_ranges`,
`batch_cells`, `batch_bytes`, `max_table_bytes`, `max_pinned_bytes` (past it no new apply starts).
Pinned bytes are counted apart from `dead_bytes`: otherwise one down node could hold
`folder.rs:429` over its limit and stop all folding.

Metrics: batches, cells finished and abandoned (by reason), eligible and ineligible cells (by
reason), apply and seal latency, tabled extents and table bytes, pinned bytes, read verification
retries.

A node removed for good keeps its ranges pinned, and with them the dead records. Because release is
per replica, healthy replicas are freed. Dropping the rest needs an operator command; not designed.

### Capability gate and downgrade

- Raft capability `CAP_INPLACE` (value 2, beside `CAP_DELTA` = 1). No new command is proposed unless
  `peers_support(CAP_DELTA | CAP_INPLACE)` in the current term. Data nodes are asked with
  `Features`.
- With `in_place` never enabled, nothing new is written to the log, the store or the wire.
- **While any table or pending apply exists, or the log may hold a new command, no older build may
  run or join** the group: it would drop the new fields, treat tabled extents as corrupt, or let a
  tier or repair command through. Atlas cannot stop an operator adding such a node; document it.
- Downgrade: disable; wait for pending applies and pinned bytes to reach 0; a background pass
  verifies each tabled extent and commits `SealExtent`; wait for the tabled count to reach 0; let
  the log compact past the last new command. A data node holds no new persistent state.

## Audit

Everything found that assumes a live extent does not change. **loss** marks a path that could lose
or corrupt acknowledged data if left as it is.

| Where | Assumption | Change |
|---|---|---|
| `metadata.rs:43`, `:91-93` | one SHA-256 describes an extent for its whole life | `blocks`, `rev`; checksum behind accessors |
| `metadata.rs:52-54`; `engine/tier.rs:5-6`, `:64-68` | never rewritten, so `created_ms` is the last change | `modified_ms` |
| `metadata.rs:647-685` (compare at `:666`) | **loss:** `TierExtent` compares only replicas; an upload read before an apply commits | `rev`, no pending apply |
| `engine/tier.rs:92-106` | the same, leader side | as above |
| `metadata.rs:686-728` | **loss:** `ReplaceReplica` installs a copy read earlier | `rev`, no pending apply |
| `engine.rs:1503-1555` (check `:1514-1518`) | **loss:** repair's "unchanged" is the replica list; the write lock does not exclude an apply | compare `rev` and `apply` |
| `engine/locality.rs:341-397` (check `:349`) | **loss:** pin copies, then commits on the replica list | as above |
| `engine.rs:1425-1498` | a replica failing the checksum is bad | block verification; skip pending; re-check |
| `metadata/delta.rs:413-456`, `gc.rs:14-20` | **loss:** a dead extent has no writer, its space can be reused | pin until sealed |
| `engine/inflight.rs:143-158`, `:192-200`; `engine.rs:2038-2043` | only a placement writes a range, and it holds a lease | applies write allocated ranges; covered by pinning |
| `data_node.rs:21-25`, `:300-353` | fencing by term is enough against late writes | apply floor and `Seal` |
| `data_node.rs:1466-1503` | retrying a write is harmless | no retry for `Apply` |
| `device.rs:103-106`, `:193-204` | acknowledged ranges are unaffected by later failures; one sync per write | qualify; add batched apply |
| `sync_guard.rs:90-93` | contract: the caller's own writes | holds; keep writes and sync on one thread |
| `engine.rs:2293-2313` | a mismatch means the extent moved (`now != ext`) | mask, barrier and retry |
| `engine.rs:2323-2325` | an object matches the whole checksum | only sealed extents are tiered |
| `engine.rs:2336-2379` (`:2365`) | verify the whole payload | covered-block mask |
| `engine.rs:2081-2119`, `engine/delta.rs:460-518` | a view's base is stable bytes | pass the covered blocks to verification; retry `:468-479` adds the barrier |
| `engine.rs:1320-1339` (no `source`) | a snapshot read never needs a second look | holds by I3; a concurrently deleted snapshot may now fail |
| `engine.rs:844-849`, `:868-906` | content hash to extent stays true | exclude tabled and pending |
| `metadata.rs:826-850` | **loss:** a share is checked by checksum equality | reject tabled and pending |
| `engine/export.rs:228-246`, `:262-269` | blob name known before reading (`content_sha`) | read first for tabled extents |
| `engine/replica.rs:110-160` | same id, same bytes | I3; add a test |
| `engine/files.rs:671-716` | a client can verify `checksum` of listed replicas | hide replicas of tabled extents |
| `atlas-native-fuse/src/ops.rs:1022-1076` | whole-extent SHA-256 | falls back to the leader through the rule above |
| `metadata/delta.rs:291-326` | the fold's compare-and-swap on base id means same content | holds: an in-place finish removes the prefix a stale fold would name, and I6 |
| `engine/rebuild.rs:62-96`, `rebuild.rs:171-172` | every listed extent can be repaired now | filter pending and pinned |
| `node.rs:1204-1210`, `node/fs_api.rs:33-45` | a lagging replica reads old data correctly | retryable error on `stale=1` |
| `raft_snapshot.rs:30` | 4096 extents make a modest chunk | up to about 22 MiB with 4 MiB tables (limit 256 MiB); size by bytes |
| `raft.rs:1005`, `:1213` | cloning the catalog is affordable | shared tables |
| `metadata/delta.rs:477-565` | invariant check | add I3, pinned and table totals |
| `docs/NATIVE_STORAGE.md:12`, `docs/NATIVE_NODE.md:147`, `docs/NATIVE_STORAGE_RESEARCH.md:174` | "never overwritten in place", "reads go on", content-addressed node cache | update; a future cache must key on `(id, rev)` |

Line numbers move; commit 2 below makes the compiler find checksum users again. The audit came from
reading and searching, so treat it as a starting list, not proof.

## Test plan

Nothing runs on the developer machine; use the lab build slot.

**Catalog.** Every command: each rejection leaves the catalog byte for byte unchanged; skipped items
skip identically; log replay, store reload and streamed snapshot agree; invariants after every step.
Interleave every existing command between Begin and Finish.

**Data node.** Fence, floor, grant, bounds. `Seal` waits for an apply stalled in a test
`DeviceSync`, and refuses it afterwards. Sync failure and mid-batch write error. Old node drops the
request.

**Torn writes.** A test `BlockStore` that records unsynced writes and, on a simulated power cut,
keeps an arbitrary subset at 512-byte granularity, then reopens. Every acknowledged byte must read
back.

**Halts.** A hook point before and after each of steps 1 to 6 and after each node's apply. At each:
kill the leader; kill, partition or stall one replica; move leadership away and back; reopen all.
Assert: model equality, a scrub with nothing bad, no pending apply or pinned bytes once nodes are
back.

**Late writes.** Delay an apply past abandonment, reclaim and reallocation: the space must not be
reused before `Seal`, and the delayed write must be refused. Repeat across a leader change.

**Mutation checks.** Each of these, disabled, must make some test fail: the `rev` compare in tier
and in repair, reclaim pinning, the covered-block mask, the barrier retry, `check_shared`,
`Features` gating, no-retry.

**Model.** Fixed-seed random runs against an in-memory model: aligned and unaligned writes,
truncate, unlink, snapshot, clone, snapshot delete, rename, tier, repair, pin, dedup on,
compression on, export and import, replication to a second cluster with failover and failback, node
failures, leader changes, copy and in-place folds chosen at random. Compare every byte, including
snapshots, after each phase and after reopen.

**Concurrency.** Readers (leader, follower, snapshot, `stale=1`) against stalled applies: never a
wrong byte; errors only where documented.

**Compatibility.** Capability gate with a node advertising nothing; mode off writes no new field
(compare serialized forms); downgrade to the base build after sealing, with real binaries.

**Existing tests** again with a test feature that applies in place whenever eligible.

**Lab, after the above:** `fio` crc32c verify through a kernel mount with node kills; `git clone`
and `fsck`. Real power loss cannot be tested on the shared host; say so.

## Commits

Each builds, passes the tests alone, and keeps the mode off. Commits 1 and 2 are useful without
phase 3.

| # | Commit | Test |
|---|---|---|
| 0 | Probe script for E1 and E2 beside `disk-probe.sh`; run E0 to E2 | results recorded here; **decision** |
| 1 | Batched copy-fold placement and its metrics | folding tests, model, one benchmark |
| 2 | Checksum accessors on `ExtentRef`; verification takes a covered mask (always empty). No behaviour change | existing tests |
| 3 | `BlockSums`, `rev`, `apply`, four commands, changed rules, invariants; nothing proposes them | catalog, replay, store, snapshot |
| 4 | Every consumer handles tabled and pending extents: reads and barrier retry, scrub, repair, pin, tier, rebuild, GC, dedup, export, layout | tests inject catalog states and damage covered blocks |
| 5 | `FileDevice` apply, `Features`, `Apply`, `Seal`, grant, client without retry | data node tests, torn-write device |
| 6 | `CAP_INPLACE`, configuration, totals, metrics | gate tests |
| 7 | Engine: eligibility, batch (steps 1 to 4), abandonment and release (5, 6), hooks | halt and late-write tests |
| 8 | Model, concurrency and mutation tests; test feature over the existing suite | all |
| 9 | `SealExtent` pass, downgrade procedure, documents | downgrade run |
| 10 | Optional: aligned placement, if the eligible fraction is low | eligibility metric |
| 11 | Optional: partial block reads, then block tables in `file_layout` for direct readers | read tests, benchmark |

Commit 4 must land before commit 7: no state may exist that a consumer mishandles.

## Not covered

- Performance of any part: every gain above is a model or arithmetic until measured.
- In place for erasure-coded, compressed, tiered, shared, block-aligned extents.
- Flash and multi-host behaviour.
- An operator command to drop ranges pinned on a node that will never return.
- Write latency at queue depth 1 and fewer syncs per write (phase 4), which E0 may show to be the
  more useful work.
