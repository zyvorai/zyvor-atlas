// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Committing folds in batches (`docs/NATIVE_DELTA_WRITES.md`, "Batched commits"): a folder
//! hands each rewritten cell to the queue of its engine and goes on to the next one, while one
//! committer thread proposes whatever is waiting as one `FoldCells` command. The committer never
//! waits for a batch to fill.
//!
//! A queued fold keeps the claim on its cell and the lease on its new space until it has ended.
//! The committer never waits for the write lock or for a claim, so a snapshot may wait for a
//! queued fold while it holds the write lock. A fold nobody can tell the outcome of (a timeout,
//! lost leadership, a failed log write, a committer that panicked) keeps its space reserved.

use std::{
    collections::BTreeSet,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Condvar, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use super::{
    delta::{is_rejected, DeltaPoint},
    folder::{backoff, Claim},
    inflight::{Lease, QUARANTINE},
    Meta, NativeEngine, NativeError,
};
use crate::{
    metadata::{Catalog, CellFold, MetaCommand},
    raft::{RaftError, CAP_FOLD_BATCH},
};

/// Upper bound of `delta_writes.fold_batch_cells`.
pub const MAX_FOLD_BATCH: usize = 256;

/// A cell whose new content is on the data nodes and is not committed yet.
pub(super) struct Rewrite<'a> {
    pub fold: CellFold,
    /// The term the content was written under.
    pub fence: u64,
    /// Holds the content's space.
    pub alloc: Lease<'a>,
}

/// How one fold of a batch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FoldEnd {
    Folded,
    /// The cell changed first; the fold certainly did not apply.
    Changed,
    /// Not committed, or nobody can tell.
    Failed,
}

/// Keeps the space of a batch whose commit never reached a decision (a panic).
struct Undecided<'r, 'a> {
    rewrites: &'r [Rewrite<'a>],
    undecided: bool,
}

impl Drop for Undecided<'_, '_> {
    fn drop(&mut self) {
        if self.undecided {
            for r in self.rewrites {
                r.alloc.quarantine(QUARANTINE);
            }
        }
    }
}

impl NativeEngine {
    /// Whether this engine may commit several folds as one command now: delta writes and
    /// batching are on, and every metadata node this leader replicates to decodes the command
    /// (false on a follower).
    pub fn fold_batch_supported(&self) -> Result<bool, NativeError> {
        if !self.cfg.delta_writes.batches() {
            return Ok(false);
        }
        Ok(match &self.meta {
            Meta::Local { .. } => true,
            Meta::Raft { server, .. } => server.peers_support(CAP_FOLD_BATCH)?,
        })
    }

    /// Commits `rewrites` and settles their space: as one command in the term this node leads,
    /// leaving out cells that changed and content written under another term, or one command
    /// each if a batch may not be proposed (any more). Returns how each ended, in order.
    pub(super) fn commit_rewrites(&self, rewrites: &[Rewrite]) -> Vec<FoldEnd> {
        if !self.fold_batch_supported().unwrap_or(false) {
            return rewrites
                .iter()
                .map(|r| match self.commit_rewrite(r) {
                    Ok(true) => FoldEnd::Folded,
                    Ok(false) => FoldEnd::Changed,
                    Err(_) => FoldEnd::Failed,
                })
                .collect();
        }
        let started = Instant::now();
        let mut open = Undecided {
            rewrites,
            undecided: true,
        };
        self.delta_point(DeltaPoint::FoldBeforePropose);
        let mut taken = vec![false; rewrites.len()];
        let mut changed = vec![false; rewrites.len()];
        let term = match &self.meta {
            Meta::Local { .. } => Ok(0),
            Meta::Raft { server, .. } => server
                .leader_term()
                .and_then(|t| t.ok_or(RaftError::NotLeader { leader: None }))
                .map_err(NativeError::from),
        };
        let proposal = term.and_then(|term| {
            // Runs under the lock the command is proposed under: a cell that changed is left
            // out here. Every replica would skip it too.
            self.commit_with(term, |c: &Catalog| {
                let mut cells = BTreeSet::new();
                let mut folds = Vec::new();
                for (i, r) in rewrites.iter().enumerate() {
                    if r.fence != term {
                        continue;
                    }
                    let f = &r.fold;
                    if cells.insert(&f.cell)
                        && c.check_fold(&f.cell, &f.base, &f.consumed, &f.extent)
                            .is_ok()
                    {
                        taken[i] = true;
                        folds.push(f.clone());
                    } else {
                        changed[i] = true;
                    }
                }
                (!folds.is_empty()).then_some(MetaCommand::FoldCells { folds })
            })
        });
        let applied = proposal.and_then(|proposed| {
            let cells = taken.iter().filter(|t| **t).count();
            if cells > 0 {
                self.telemetry.fold_batch.observe(cells as u64);
                self.delta_point(DeltaPoint::FoldProposed);
            }
            self.await_commit(proposed)
        });
        open.undecided = false;
        let telemetry = &self.telemetry;
        rewrites
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let end = match &applied {
                    _ if changed[i] => FoldEnd::Changed,
                    // Not proposed: its space is free again once the lease goes.
                    _ if !taken[i] => FoldEnd::Failed,
                    Ok(()) => FoldEnd::Folded,
                    Err(e) => {
                        r.alloc.settle(e);
                        if is_rejected(e) {
                            FoldEnd::Changed
                        } else {
                            FoldEnd::Failed
                        }
                    }
                };
                match end {
                    FoldEnd::Folded => telemetry.delta_folded(r.fold.consumed.len()),
                    FoldEnd::Changed => telemetry.delta_fold_conflict(),
                    FoldEnd::Failed => {}
                }
                telemetry.fold_commit.observe(started.elapsed());
                end
            })
            .collect()
    }
}

struct Waiting<'a> {
    /// The queued folds, and the claim on each one's cell.
    rewrites: Vec<Rewrite<'a>>,
    claims: Vec<Claim<'a>>,
    /// Folders that may still hand something over.
    producers: usize,
    /// There is no committer.
    closed: bool,
    /// Batches in a row with a failed fold, and until when the folders leave this engine alone
    /// because of it.
    failures: u32,
    held: Option<Instant>,
}

/// The folds of one engine that wait to be committed: at most `cap`, beside the (at most `cap`)
/// being committed. A folder with a rewrite to hand over to a full queue waits.
pub(super) struct FoldQueue<'a> {
    state: Mutex<Waiting<'a>>,
    changed: Condvar,
    cap: usize,
}

impl<'a> FoldQueue<'a> {
    pub(super) fn new(cap: usize, producers: usize) -> Self {
        Self {
            state: Mutex::new(Waiting {
                rewrites: Vec::new(),
                claims: Vec::new(),
                producers,
                closed: false,
                failures: 0,
                held: None,
            }),
            changed: Condvar::new(),
            cap,
        }
    }

    fn state(&self) -> MutexGuard<'_, Waiting<'a>> {
        // Nothing panics while holding it, and it must stay usable while unwinding.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hands a rewrite over, waiting for room. Returns it if nothing will commit it.
    #[allow(clippy::type_complexity)]
    pub(super) fn push(
        &self,
        rewrite: Rewrite<'a>,
        claim: Claim<'a>,
    ) -> Option<(Rewrite<'a>, Claim<'a>)> {
        let mut s = self.state();
        while !s.closed && s.rewrites.len() >= self.cap {
            s = self.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
        if s.closed {
            return Some((rewrite, claim));
        }
        s.rewrites.push(rewrite);
        s.claims.push(claim);
        self.changed.notify_all();
        None
    }

    /// Everything waiting, as soon as there is anything. `None`: nothing waits and no folder
    /// is left to hand over more.
    #[allow(clippy::type_complexity)]
    fn take(&self) -> Option<(Vec<Rewrite<'a>>, Vec<Claim<'a>>)> {
        let mut s = self.state();
        while s.rewrites.is_empty() {
            if s.producers == 0 {
                return None;
            }
            s = self.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
        let batch = (
            std::mem::take(&mut s.rewrites),
            std::mem::take(&mut s.claims),
        );
        self.changed.notify_all();
        Some(batch)
    }

    /// A folder is gone.
    pub(super) fn producer_done(&self) {
        let mut s = self.state();
        s.producers = s.producers.saturating_sub(1);
        self.changed.notify_all();
    }

    /// Nothing will commit what is queued (not proposed, so its space is free again).
    pub(super) fn close(&self) {
        let mut s = self.state();
        s.closed = true;
        s.rewrites.clear();
        s.claims.clear();
        self.changed.notify_all();
    }

    /// Whether the folders should start nothing new for this engine because commits fail.
    pub(super) fn held(&self) -> bool {
        self.state()
            .held
            .is_some_and(|until| Instant::now() < until)
    }

    /// Takes note of a batch, holding the folders off longer each time one fails in a row.
    fn committed(&self, failed: bool, interval: Duration) {
        let mut s = self.state();
        if failed {
            s.failures = s.failures.saturating_add(1);
            s.held = Some(Instant::now() + backoff(interval, s.failures));
        } else {
            s.failures = 0;
            s.held = None;
        }
    }
}

/// Closes the queue if its committer ends in any way, so no folder waits for it for ever.
struct Closing<'q, 'a>(&'q FoldQueue<'a>);

impl Drop for Closing<'_, '_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// The committer of one engine's queue: proposes whatever the folders have handed over, until
/// they are all gone and nothing waits. `done` hears of every fold, and whether it did not fail.
pub(super) fn commit_queue<'a>(
    engine: &'a NativeEngine,
    queue: &FoldQueue<'a>,
    interval: Duration,
    done: &(impl Fn(bool) + Sync),
) {
    let _closing = Closing(queue);
    while let Some((rewrites, claims)) = queue.take() {
        // A panic must not take the committer with it; the batch keeps its space.
        let ends = catch_unwind(AssertUnwindSafe(|| engine.commit_rewrites(&rewrites)))
            .unwrap_or_else(|_| vec![FoldEnd::Failed; rewrites.len()]);
        // Space first: once a cell can be claimed again, its last fold is fully settled.
        drop(rewrites);
        drop(claims);
        queue.committed(ends.contains(&FoldEnd::Failed), interval);
        for end in ends {
            done(end != FoldEnd::Failed);
        }
    }
}

#[cfg(test)]
mod tests;
