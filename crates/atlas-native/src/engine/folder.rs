// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Folding many cells (`docs/NATIVE_DELTA_WRITES.md`, "How folding works"): which cell to fold
//! next, keeping two folds off the same cell, the pool of threads that folds in the background,
//! and the early reclaim that runs beside it. The pool's commits are batched by
//! [`super::fold_batch`].
//!
//! Every fold goes through a [`Claim`] on its cell, held until the fold has ended. Nothing that
//! a held claim depends on (its folder, the queue it waits in, the committer) waits for the
//! write lock or for a second claim, so a snapshot may wait for a claim while it holds the write
//! lock. The catalog's compare-and-swap is what keeps data right; claims only keep two folds
//! from doing the same work, and let a snapshot wait for a fold instead of being refused.

use std::{
    collections::{BinaryHeap, HashMap, HashSet, VecDeque},
    ops::Bound,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Condvar, Mutex, MutexGuard, PoisonError,
    },
    thread,
    time::{Duration, Instant},
};

use super::{
    fold_batch::{commit_queue, FoldQueue},
    Meta, NativeEngine, NativeError,
};
use crate::{gc::GcStats, metadata::DeltaCell, raft::RaftError};

/// Upper bound of `delta_writes.fold_concurrency`. Each fold in progress holds a whole cell in
/// memory and runs its own reader and replica writer threads.
pub const MAX_FOLD_CONCURRENCY: usize = 32;
/// Cells one plan queues in each lane.
const PLAN_CELLS: usize = 256;
/// Dirty cells looked at per hold of the catalog lock (under Raft, the node's lock).
const SCAN_CHUNK: usize = 4096;
/// One pick in this many takes the cell that has waited longest, whatever it holds.
const OLDEST_EVERY: u64 = 4;
/// A queue older than this is planned again, so that the order follows the writes.
const REPLAN: Duration = Duration::from_secs(1);
/// Cells of a scope being snapshotted that one pick looks through for a free one.
const BLOCKED_LOOK: usize = 2 * MAX_FOLD_CONCURRENCY;
/// Longest pause of a folder after repeated failures (or the folding interval, if longer).
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// How often a sleeping folder looks at the stop flag.
const NAP: Duration = Duration::from_millis(20);

/// The `PLAN_CELLS` lowest-ranked cells offered.
struct Lowest<R: Ord>(BinaryHeap<(R, DeltaCell)>);

impl<R: Ord + Copy> Lowest<R> {
    fn new() -> Self {
        Self(BinaryHeap::with_capacity(PLAN_CELLS + 1))
    }

    /// Clones `key` only if it is among the lowest so far.
    fn offer(&mut self, rank: R, key: &DeltaCell) {
        if self.0.len() >= PLAN_CELLS {
            match self.0.peek() {
                Some((r, k)) if (rank, key) < (*r, k) => {
                    self.0.pop();
                }
                _ => return,
            }
        }
        self.0.push((rank, key.clone()));
    }

    fn into_queue(self) -> VecDeque<DeltaCell> {
        self.0.into_sorted_vec().into_iter().map(|e| e.1).collect()
    }
}

/// What makes a dirty cell due in one plan.
struct Rules<'a> {
    now: Instant,
    /// Everything is due: delta writes are off, or their room is half used.
    all: bool,
    after_deltas: u32,
    after: Duration,
    /// Cells being folded already.
    busy: &'a HashSet<DeltaCell>,
}

/// A plan being made.
struct Pass {
    /// Most deltas first, the longest waiting first among equals.
    most: Lowest<(std::cmp::Reverse<u32>, Instant)>,
    oldest: Lowest<Instant>,
    due: usize,
}

#[derive(Debug, Default)]
struct Planner {
    /// Dirty cells: when this node first saw each dirty since it was last picked, and the last
    /// plan that saw it.
    seen: HashMap<DeltaCell, (Instant, u64)>,
    plans: u64,
    most: VecDeque<DeltaCell>,
    oldest: VecDeque<DeltaCell>,
    picks: u64,
    planned: Option<Instant>,
    /// The last plan found more cells due than it queued.
    more: bool,
}

impl Planner {
    /// Whether to plan before the next pick: never planned, out of cells (at once if the last
    /// plan left some out, else after `idle`), or a queue that has grown old.
    fn stale(&self, now: Instant, idle: Duration) -> bool {
        let Some(at) = self.planned else {
            return true;
        };
        let age = now.saturating_duration_since(at);
        if self.most.is_empty() && self.oldest.is_empty() {
            self.more || age >= idle
        } else {
            age >= REPLAN
        }
    }

    fn begin(&mut self) -> Pass {
        self.plans += 1;
        Pass {
            most: Lowest::new(),
            oldest: Lowest::new(),
            due: 0,
        }
    }

    /// Looks at some of the dirty cells and their delta counts.
    fn look<'a>(
        &mut self,
        pass: &mut Pass,
        rules: &Rules,
        cells: impl Iterator<Item = (&'a DeltaCell, u32)>,
    ) {
        for (key, count) in cells {
            let since = match self.seen.get_mut(key) {
                Some((since, plan)) => {
                    *plan = self.plans;
                    *since
                }
                None => {
                    self.seen.insert(key.clone(), (rules.now, self.plans));
                    rules.now
                }
            };
            let due = rules.all
                || count >= rules.after_deltas
                || rules.now.saturating_duration_since(since) >= rules.after;
            if due && !rules.busy.contains(key) {
                pass.due += 1;
                pass.most.offer((std::cmp::Reverse(count), since), key);
                pass.oldest.offer(since, key);
            }
        }
    }

    /// Takes the plan made, once every dirty cell has been looked at.
    fn end(&mut self, pass: Pass, now: Instant) {
        // Cells no longer dirty are forgotten.
        let plans = self.plans;
        self.seen.retain(|_, (_, plan)| *plan == plans);
        self.more = pass.due > PLAN_CELLS;
        self.most = pass.most.into_queue();
        self.oldest = pass.oldest.into_queue();
        self.planned = Some(now);
    }

    /// The next queued cell `claim` accepts: usually the one with most deltas, every
    /// [`OLDEST_EVERY`]th time the one waiting longest, so that none waits for ever.
    fn pick<C>(
        &mut self,
        mut claim: impl FnMut(&DeltaCell) -> Option<C>,
    ) -> Option<(DeltaCell, C)> {
        loop {
            let (first, second) = if self.picks % OLDEST_EVERY == OLDEST_EVERY - 1 {
                (&mut self.oldest, &mut self.most)
            } else {
                (&mut self.most, &mut self.oldest)
            };
            let key = first.pop_front().or_else(|| second.pop_front())?;
            // Not there: picked from the other queue already.
            if !self.seen.contains_key(&key) {
                continue;
            }
            let Some(claimed) = claim(&key) else {
                continue;
            };
            // Its wait starts again if it is still, or again, dirty.
            self.seen.remove(&key);
            self.picks += 1;
            return Some((key, claimed));
        }
    }
}

#[derive(Debug, Default)]
struct Claims {
    held: HashSet<DeltaCell>,
    /// Cells someone is waiting to claim, which therefore are not given to anyone else first.
    wanted: HashMap<DeltaCell, usize>,
}

/// Leader-local state of folding; none of it is replicated.
#[derive(Debug, Default)]
pub(super) struct Folds {
    planner: Mutex<Planner>,
    claims: Mutex<Claims>,
    released: Condvar,
}

/// The right to fold one cell, until dropped.
pub(super) struct Claim<'a> {
    folds: &'a Folds,
    key: DeltaCell,
}

impl Folds {
    fn claims(&self) -> MutexGuard<'_, Claims> {
        // Nothing panics while holding it, and a claim must be released while unwinding.
        self.claims.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn claimed(&self) -> HashSet<DeltaCell> {
        self.claims().held.clone()
    }

    /// The claim on `key`, unless it is held or waited for.
    pub(super) fn try_claim(&self, key: &DeltaCell) -> Option<Claim<'_>> {
        let mut c = self.claims();
        if c.held.contains(key) || c.wanted.contains_key(key) {
            return None;
        }
        c.held.insert(key.clone());
        Some(Claim {
            folds: self,
            key: key.clone(),
        })
    }

    /// The claim on `key`, waiting for whoever holds it.
    pub(super) fn claim(&self, key: &DeltaCell) -> Claim<'_> {
        let mut c = self.claims();
        *c.wanted.entry(key.clone()).or_default() += 1;
        while c.held.contains(key) {
            c = self
                .released
                .wait(c)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if let Some(n) = c.wanted.get_mut(key) {
            *n -= 1;
            if *n == 0 {
                c.wanted.remove(key);
            }
        }
        c.held.insert(key.clone());
        Claim {
            folds: self,
            key: key.clone(),
        }
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.folds.claims().held.remove(&self.key);
        self.folds.released.notify_all();
    }
}

/// Sleeps for `time`, or until `stop` is set.
pub(crate) fn nap(time: Duration, stop: &AtomicBool) {
    let start = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        let left = time.saturating_sub(start.elapsed());
        if left.is_zero() {
            break;
        }
        thread::sleep(left.min(NAP));
    }
}

/// The pause after `failures` failed folds in a row.
pub(super) fn backoff(interval: Duration, failures: u32) -> Duration {
    let doubled = 1u32 << failures.saturating_sub(1).min(16);
    interval
        .saturating_mul(doubled)
        .min(MAX_BACKOFF.max(interval))
}

/// Folds the due cells of `engines` (the metadata groups of one node) on
/// `delta_writes.fold_concurrency` threads, so at most that many cells are being rewritten at
/// once over all of them, until `stop` is set. Returns once the folds in progress have ended. An
/// engine that batches its folds (`fold_batch_cells` above 1, delta writes on) also gets a
/// committer thread; the folders hand it each rewritten cell and go on, and it holds at most
/// twice `fold_batch_cells` folds. A folder with nothing to do looks again after
/// `fold_interval_ms`; one whose fold failed pauses, longer each time it fails again, and so do
/// all of them for an engine whose batches fail. `done` hears of every fold attempted, and
/// whether it did not fail.
///
/// The pool's settings are those of the first engine.
pub fn run_folders<'e>(
    engines: &[&'e NativeEngine],
    stop: &AtomicBool,
    done: impl Fn(bool) + Sync,
) {
    let Some(first) = engines.first() else {
        return;
    };
    let cfg = &first.cfg.delta_writes;
    let interval = Duration::from_millis(cfg.fold_interval_ms);
    let queues: Vec<Option<FoldQueue<'e>>> = engines
        .iter()
        .map(|e| {
            let c = &e.cfg.delta_writes;
            c.batches()
                .then(|| FoldQueue::new(c.fold_batch_cells, cfg.fold_concurrency))
        })
        .collect();
    thread::scope(|s| {
        for (&engine, queue) in engines.iter().zip(&queues) {
            let Some(queue) = queue else {
                continue;
            };
            let done = &done;
            let spawned = thread::Builder::new()
                .name("atlas-fold-commit".into())
                .spawn_scoped(s, move || commit_queue(engine, queue, interval, done));
            if spawned.is_err() {
                // The folders then commit their own folds.
                queue.close();
                done(false);
            }
        }
        for i in 0..cfg.fold_concurrency {
            let (done, queues) = (&done, &queues);
            // Counted off however the folder ends, also if it never starts.
            let producer = Producer(queues);
            let spawned = thread::Builder::new()
                .name(format!("atlas-fold-{i}"))
                .spawn_scoped(s, move || {
                    let _producer = producer;
                    folder(engines, queues, i, interval, stop, done)
                });
            if spawned.is_err() {
                done(false);
            }
        }
    });
}

/// One folder's place among those the committers wait for.
struct Producer<'q, 'e>(&'q [Option<FoldQueue<'e>>]);

impl Drop for Producer<'_, '_> {
    fn drop(&mut self) {
        for queue in self.0.iter().flatten() {
            queue.producer_done();
        }
    }
}

/// What one turn of a folder did.
enum Turn {
    /// Nothing was due.
    Idle,
    /// A rewritten cell went to the committer, which reports how it ends.
    Handed,
    /// A fold ran to its end here.
    Ended,
}

/// One thread of [`run_folders`], taking the engines in turn from `at`.
fn folder<'e>(
    engines: &[&'e NativeEngine],
    queues: &[Option<FoldQueue<'e>>],
    mut at: usize,
    interval: Duration,
    stop: &AtomicBool,
    done: &(impl Fn(bool) + Sync),
) {
    let mut failures = 0u32;
    while !stop.load(Ordering::SeqCst) {
        let mut turn = None;
        for _ in 0..engines.len() {
            let i = at % engines.len();
            let (engine, queue) = (engines[i], queues[i].as_ref());
            at = at.wrapping_add(1);
            // Its commits failed lately: nothing new for it yet.
            if queue.is_some_and(FoldQueue::held) {
                continue;
            }
            // A panic must not take the folder with it. The claim and the space the fold held
            // are released as it unwinds.
            match catch_unwind(AssertUnwindSafe(|| engine.fold_turn(queue))) {
                Ok(Ok(Turn::Idle)) | Ok(Err(NativeError::Raft(RaftError::NotLeader { .. }))) => {
                    continue
                }
                Ok(Ok(ran)) => turn = Some(Ok(ran)),
                Ok(Err(_)) | Err(_) => turn = Some(Err(())),
            }
            break;
        }
        match turn {
            Some(Ok(turn)) => {
                failures = 0;
                if matches!(turn, Turn::Ended) {
                    done(true);
                }
            }
            Some(Err(())) => {
                failures = failures.saturating_add(1);
                done(false);
                nap(backoff(interval, failures), stop);
            }
            None => nap(interval, stop),
        }
    }
}

impl NativeEngine {
    /// Whether this engine has folding to look after: it leads, and delta writes are on or
    /// deltas are pending.
    fn folding(&self) -> Result<bool, NativeError> {
        if let Meta::Raft { server, .. } = &self.meta {
            if server.leader_term()?.is_none() {
                return Ok(false);
            }
        }
        Ok(self.cfg.delta_writes.enabled || self.with_catalog(|c| !c.dirty.is_empty())?)
    }

    /// Looks at every dirty cell, a chunk per hold of the catalog lock, and queues the due ones.
    fn plan_folds(&self, planner: &mut Planner) -> Result<(), NativeError> {
        let cfg = &self.cfg.delta_writes;
        let busy = self.delta.folds.claimed();
        let now = Instant::now();
        let mut pass = planner.begin();
        let mut after: Option<DeltaCell> = None;
        loop {
            let next = self.with_catalog(|c| {
                let rules = Rules {
                    now,
                    // Turned off, nothing keeps deltas around: fold them all.
                    all: !cfg.enabled || self.delta_pressure(c),
                    after_deltas: cfg.fold_after_deltas,
                    after: Duration::from_millis(cfg.fold_after_ms),
                    busy: &busy,
                };
                let from = after.as_ref().map_or(Bound::Unbounded, Bound::Excluded);
                let (mut looked, mut last) = (0, None);
                let chunk = c.dirty.range((from, Bound::Unbounded)).take(SCAN_CHUNK);
                planner.look(
                    &mut pass,
                    &rules,
                    chunk.map(|(key, count)| {
                        looked += 1;
                        last = Some(key);
                        (key, *count)
                    }),
                );
                last.filter(|_| looked == SCAN_CHUNK).cloned()
            })?;
            if next.is_none() {
                break;
            }
            after = next;
        }
        planner.end(pass, Instant::now());
        Ok(())
    }

    /// A free cell of a target a snapshot is waiting to be taken of. No new delta lands there,
    /// and the write lock is probably held until they are all folded.
    fn blocked_fold(&self) -> Result<Option<(DeltaCell, Claim<'_>)>, NativeError> {
        for scope in self.blocked_scopes()? {
            let cells = self.with_catalog(|c| scope.cells(c, BLOCKED_LOOK))?;
            for key in cells {
                if let Some(claim) = self.delta.folds.try_claim(&key) {
                    return Ok(Some((key, claim)));
                }
            }
        }
        Ok(None)
    }

    /// The cell to fold next, claimed; `afresh` plans again first. None while nothing is due,
    /// or while dead extents alone take all the room delta writes have: folding more would only
    /// use more device space until reclaim catches up.
    fn next_fold(&self, afresh: bool) -> Result<Option<(DeltaCell, Claim<'_>)>, NativeError> {
        if let Some(found) = self.blocked_fold()? {
            return Ok(Some(found));
        }
        let cfg = &self.cfg.delta_writes;
        if self.with_catalog(|c| c.delta_totals.dead_bytes >= cfg.max_pending_bytes)? {
            return Ok(None);
        }
        let mut planner = self
            .delta
            .folds
            .planner
            .lock()
            .map_err(|_| NativeError::Poisoned("fold planner"))?;
        let idle = Duration::from_millis(cfg.fold_interval_ms);
        let claim = |key: &DeltaCell| self.delta.folds.try_claim(key);
        if afresh || planner.stale(Instant::now(), idle) {
            self.plan_folds(&mut planner)?;
        } else if let Some(found) = planner.pick(claim) {
            return Ok(Some(found));
        } else if !planner.stale(Instant::now(), idle) {
            return Ok(None);
        } else {
            // That emptied a queue that left cells out: no reason to wait.
            self.plan_folds(&mut planner)?;
        }
        Ok(planner.pick(claim))
    }

    /// Folds the most urgent cell due that no other fold has. `None`: nothing to do now (also on
    /// a follower). `Some(false)`: the cell changed before its fold could commit.
    pub fn fold_next(&self) -> Result<Option<bool>, NativeError> {
        if !self.folding()? {
            return Ok(None);
        }
        match self.next_fold(false)? {
            Some((key, _claim)) => self.fold_cell(&key).map(Some),
            None => Ok(None),
        }
    }

    /// Rewrites the most urgent cell due that no other fold has, and hands it to `queue` to be
    /// committed with others. Without a queue, or while a batch may not be proposed, the fold is
    /// committed here.
    fn fold_turn<'a>(&'a self, queue: Option<&FoldQueue<'a>>) -> Result<Turn, NativeError> {
        if !self.folding()? {
            return Ok(Turn::Idle);
        }
        let Some((key, claim)) = self.next_fold(false)? else {
            return Ok(Turn::Idle);
        };
        let queue = match queue {
            Some(queue) if self.fold_batch_supported()? => queue,
            _ => {
                self.fold_cell(&key)?;
                return Ok(Turn::Ended);
            }
        };
        let Some(rewrite) = self.rewrite_cell(&key)? else {
            return Ok(Turn::Ended);
        };
        match queue.push(rewrite, claim) {
            None => Ok(Turn::Handed),
            Some((rewrite, _claim)) => {
                self.commit_rewrite(&rewrite)?;
                Ok(Turn::Ended)
            }
        }
    }

    /// Folds `cells`, one at a time. A cell another fold holds is skipped, or with `wait` folded
    /// after that one ends. Returns the folds this call committed.
    pub(super) fn fold_cells(
        &self,
        cells: Vec<DeltaCell>,
        wait: bool,
    ) -> Result<usize, NativeError> {
        let mut folded = 0;
        let mut held = Vec::new();
        for key in cells {
            match self.delta.folds.try_claim(&key) {
                Some(_claim) => folded += usize::from(self.fold_cell(&key)?),
                None => held.push(key),
            }
        }
        if wait {
            for key in held {
                let _claim = self.delta.folds.claim(&key);
                folded += usize::from(self.fold_cell(&key)?);
            }
        }
        Ok(folded)
    }

    /// Reclaims dead extents early if they and the pending deltas have used up half the room
    /// delta writes have. `None` if not, on a follower, or with no delta pending and delta
    /// writes off. A node runs this on its own thread, so that folding and reclaim never wait
    /// for each other.
    pub fn reclaim_if_pressed(&self) -> Result<Option<GcStats>, NativeError> {
        if !self.folding()?
            || !self.with_catalog(|c| c.delta_totals.dead_bytes > 0 && self.delta_pressure(c))?
        {
            return Ok(None);
        }
        self.gc_once().map(Some)
    }

    /// One folding pass on the caller's thread: plans, folds at most `fold_cells_per_pass` due
    /// cells one at a time, then reclaims early if room is short. Returns the cells folded. A
    /// node does not use this: it runs [`run_folders`] and [`Self::reclaim_if_pressed`].
    pub fn delta_maintenance_once(&self) -> Result<usize, NativeError> {
        if !self.folding()? {
            return Ok(0);
        }
        let mut folded = 0;
        for i in 0..self.cfg.delta_writes.fold_cells_per_pass {
            let Some((key, _claim)) = self.next_fold(i == 0)? else {
                break;
            };
            folded += usize::from(self.fold_cell(&key)?);
        }
        self.reclaim_if_pressed()?;
        Ok(folded)
    }
}

#[cfg(test)]
mod tests;
