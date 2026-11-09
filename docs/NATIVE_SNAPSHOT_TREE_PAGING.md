<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Design note: paging filesystem snapshot trees

Status: **proposal, nothing implemented, nothing measured.** Everything under "Today" was read from the
source at the commit this page was written against (`crates/atlas-native/src/namespace.rs`, `store.rs`,
`inodes.rs`); no memory or size figure below was measured. The user-visible limit it addresses is already
listed in [NATIVE_METADATA.md](NATIVE_METADATA.md) ("filesystem snapshot trees are still held in memory in
full").

## Today

- Live filesystems are paged: inodes live in the `INODES` table and directory entries in `DIR_ENTRIES`,
  keyed by `(fs, ino)` and `(fs, dir, name)`; memory holds a bounded cache plus the entries changed since
  the last checkpoint.
- A filesystem snapshot is not. `Catalog::snapshot_tree` calls `InodeTable::detached()` (every inode, every
  directory entry in memory) and stores the result as `FsSnapshotMeta.tree`, one record per snapshot in the
  `FS_SNAPSHOTS` table.
- `Store::load` reads the whole `FS_SNAPSHOTS` table into the catalog (`read_table`) at start, so every
  snapshot tree is resident for the life of the process.
- Inside one running process the trees share inodes through `Arc`, so a snapshot of an unchanged tree is
  cheap in memory. That sharing is not persisted: each stored record is a full copy, and after a restart
  each loaded tree is its own deserialized copy. The expected consequence (not measured) is that resident
  memory after a restart grows with *snapshots x files*, not with the files that actually differ.
- Readers of a frozen tree: `Catalog::fs_view` (`<fs>@<snapshot>` reads, replication diff, S3 export, CSI
  clones) and the Raft snapshot chunker, all of which assume the tree is an in-memory `FsMeta`.

## Proposal: versioned inode records, snapshot = a cut-off

Keep one physical copy of each inode version and let a snapshot name a point in time instead of owning a
tree.

1. Add a per-filesystem monotonically increasing `tree_version` (a field next to `next_ino`).
2. Key inode and directory-entry records by `(fs, ino, version)` / `(fs, dir, name, version)`. A live write
   at version `v` writes a new record; it never overwrites the older one while a snapshot may still need it.
3. A snapshot stores `(fs, version)` plus the small per-filesystem header (counters, quota) instead of a
   tree. Reading `<fs>@<snapshot>` is a reverse range read for the newest record at or below the cut-off;
   deletes are tombstone versions.
4. Garbage collection: a record version is dead when no snapshot cut-off lies between it and the next newer
   version of the same key. Run it at checkpoint, bounded per checkpoint, and keep extent refcounts exactly
   as `share_extents` / `drop_inode` do today (extents are refcounted per inode version that references
   them).
5. Migration: keep reading the old `FS_SNAPSHOTS.tree` format. On first checkpoint after upgrade, convert
   each legacy snapshot lazily (write its tree into versioned records at a fresh cut-off) and only then drop
   the old record. A downgrade path does not exist once converted; say so in the release notes.

## Why this is not a quick change

- It changes the on-disk catalog format of a Raft-replicated store, and the Raft snapshot chunker, the
  replication diff (`/v1/fs-snapshots/{to}/diff`), snapshot export/import to S3, quotas/usage counting for
  snapshot trees and `fs_view` all read frozen trees whole today.
- A bug here loses or corrupts metadata, which is not recoverable from the data nodes alone.
- No lab host in this repository's records has the snapshot count or file count needed to show the problem
  or the fix; proving either needs a measured run.

## Before any code: measure

1. Add a read-only gauge: resident bytes and record count of `fs_snapshots` per filesystem (reuse the
   `store::bench::memory_of_a_paged_catalog` approach).
2. Run it on the lab: 100k files, 1/10/50 snapshots, before and after a restart. This confirms or refutes the
   "multiplies after restart" expectation above.
3. Only if the numbers justify it, implement the design behind a catalog format version, with the legacy
   reader kept, a round-trip test (write old, upgrade, read both), a crash-between-steps test for the lazy
   conversion, and the full `store` and replication test suites green on SQLite and the CI Postgres path.

## Not decided

- Whether versions should be per filesystem or per Raft index (per Raft index makes cut-offs trivial and
  deterministic across replicas, at the cost of larger keys).
- Whether directory entries need their own versions or can be reconstructed from inode versions.
- The GC cadence under many snapshots with long retention locks (WORM): locked snapshots pin versions for
  their whole lock period.
