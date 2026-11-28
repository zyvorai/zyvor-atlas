// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

use std::sync::atomic::{AtomicU64, Ordering};

use crate::metrics::Histogram;

#[derive(Debug, Default)]
pub struct NativeIoCounters {
    reads: AtomicU64,
    writes: AtomicU64,
    read_bytes: AtomicU64,
    write_bytes: AtomicU64,
    checksum_failures: AtomicU64,
    replica_fallbacks: AtomicU64,
    gc_reclaimed: AtomicU64,
    replica_write_failures: AtomicU64,
    replicas_repaired: AtomicU64,
    extents_tiered: AtomicU64,
    object_reads: AtomicU64,
    dedup_hits: AtomicU64,
    dedup_saved_bytes: AtomicU64,
    delta_writes: AtomicU64,
    delta_folds: AtomicU64,
    delta_fallbacks: AtomicU64,
    delta_fold_conflicts: AtomicU64,
    delta_folded_deltas: AtomicU64,
    concurrent_commits: AtomicU64,
    concurrent_conflicts: AtomicU64,
    /// Where a fold's time goes: reading the cell, placing the new base on its replicas, and
    /// committing it. Failed attempts count.
    pub fold_read: Histogram,
    pub fold_write: Histogram,
    pub fold_commit: Histogram,
    /// Where a delta write's time goes: placing its bytes on the replicas, and getting them
    /// committed (the wait for a delta command included). Failed attempts count.
    pub delta_replicate: Histogram,
    pub delta_commit: Histogram,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeIoSnapshot {
    pub reads: u64,
    pub writes: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub checksum_failures: u64,
    pub replica_fallbacks: u64,
    pub gc_reclaimed: u64,
    pub replica_write_failures: u64,
    pub replicas_repaired: u64,
    pub extents_tiered: u64,
    pub object_reads: u64,
    /// Extent writes that referenced an existing identical extent, and the bytes they avoided.
    pub dedup_hits: u64,
    pub dedup_saved_bytes: u64,
    /// Writes committed as deltas, cells folded, and writes that were eligible but rewrote
    /// their cell instead (over a cap, a target being snapshotted, a rejected commit).
    pub delta_writes: u64,
    pub delta_folds: u64,
    pub delta_fallbacks: u64,
    /// Folds the catalog rejected, and the deltas the successful ones consumed.
    pub delta_fold_conflicts: u64,
    pub delta_folded_deltas: u64,
    /// Writes committed concurrently, and those whose cell changed before they could be
    /// proposed.
    pub concurrent_commits: u64,
    pub concurrent_conflicts: u64,
}

impl NativeIoCounters {
    pub fn record_read(&self, bytes: usize) {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.read_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
    pub fn record_write(&self, bytes: usize) {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.write_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
    pub fn dedup_hit(&self, bytes: usize) {
        self.dedup_hits.fetch_add(1, Ordering::Relaxed);
        self.dedup_saved_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }
    pub fn checksum_failure(&self) {
        self.checksum_failures.fetch_add(1, Ordering::Relaxed);
    }
    pub fn replica_fallback(&self) {
        self.replica_fallbacks.fetch_add(1, Ordering::Relaxed);
    }
    pub fn gc_reclaimed(&self, extents: u64) {
        self.gc_reclaimed.fetch_add(extents, Ordering::Relaxed);
    }
    /// A replica write that failed and was retried on another node.
    pub fn replica_write_failure(&self) {
        self.replica_write_failures.fetch_add(1, Ordering::Relaxed);
    }
    pub fn replica_repaired(&self) {
        self.replicas_repaired.fetch_add(1, Ordering::Relaxed);
    }
    pub fn extent_tiered(&self) {
        self.extents_tiered.fetch_add(1, Ordering::Relaxed);
    }
    pub fn object_read(&self) {
        self.object_reads.fetch_add(1, Ordering::Relaxed);
    }
    pub fn delta_written(&self) {
        self.delta_writes.fetch_add(1, Ordering::Relaxed);
    }
    pub fn delta_folded(&self, deltas: usize) {
        self.delta_folds.fetch_add(1, Ordering::Relaxed);
        self.delta_folded_deltas
            .fetch_add(deltas as u64, Ordering::Relaxed);
    }
    pub fn delta_fold_conflict(&self) {
        self.delta_fold_conflicts.fetch_add(1, Ordering::Relaxed);
    }
    pub fn delta_fallback(&self) {
        self.delta_fallbacks.fetch_add(1, Ordering::Relaxed);
    }
    pub fn concurrent_commit(&self) {
        self.concurrent_commits.fetch_add(1, Ordering::Relaxed);
    }
    pub fn concurrent_conflict(&self) {
        self.concurrent_conflicts.fetch_add(1, Ordering::Relaxed);
    }
    pub fn snapshot(&self) -> NativeIoSnapshot {
        NativeIoSnapshot {
            reads: self.reads.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            read_bytes: self.read_bytes.load(Ordering::Relaxed),
            write_bytes: self.write_bytes.load(Ordering::Relaxed),
            checksum_failures: self.checksum_failures.load(Ordering::Relaxed),
            replica_fallbacks: self.replica_fallbacks.load(Ordering::Relaxed),
            gc_reclaimed: self.gc_reclaimed.load(Ordering::Relaxed),
            replica_write_failures: self.replica_write_failures.load(Ordering::Relaxed),
            replicas_repaired: self.replicas_repaired.load(Ordering::Relaxed),
            extents_tiered: self.extents_tiered.load(Ordering::Relaxed),
            object_reads: self.object_reads.load(Ordering::Relaxed),
            dedup_hits: self.dedup_hits.load(Ordering::Relaxed),
            dedup_saved_bytes: self.dedup_saved_bytes.load(Ordering::Relaxed),
            delta_writes: self.delta_writes.load(Ordering::Relaxed),
            delta_folds: self.delta_folds.load(Ordering::Relaxed),
            delta_fallbacks: self.delta_fallbacks.load(Ordering::Relaxed),
            delta_fold_conflicts: self.delta_fold_conflicts.load(Ordering::Relaxed),
            delta_folded_deltas: self.delta_folded_deltas.load(Ordering::Relaxed),
            concurrent_commits: self.concurrent_commits.load(Ordering::Relaxed),
            concurrent_conflicts: self.concurrent_conflicts.load(Ordering::Relaxed),
        }
    }
}
