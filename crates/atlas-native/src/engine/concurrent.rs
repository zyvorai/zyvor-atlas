// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Concurrent commits (`metadata.concurrent_commits`, off by default): a write that creates a
//! cell's extent or replaces all of it needs nothing the cell holds, so it places its data and
//! waits for its commit without the write lock. See "Concurrent commits" in
//! `docs/NATIVE_METADATA.md`.
//!
//! What orders such a write:
//!
//! - writes of the same cell take turns ([`CellTurns`]), from planning until their commit has
//!   applied;
//! - the write lock is held only while the command is proposed. Whoever holds it (a partial
//!   overwrite, a truncate, a snapshot that folds deltas) therefore knows that none is proposed
//!   meanwhile, and `write_fence` then covers those proposed before;
//! - the proposal is a compare-and-swap: it is made only if, in the catalog it will apply on,
//!   every cell is still one the write may replace without reading it. Otherwise nothing is
//!   proposed and the write takes the locked path, which reads and merges.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
};

use super::{inflight, is_stale_share, NativeEngine, NativeError, Proposed, Target};
use crate::metadata::{Catalog, DeltaCell, DeltaTarget, ExtentId, ExtentRef};

/// A step of a concurrent write, for tests that act between steps.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitPoint {
    /// The bytes are on the data nodes; nothing is proposed.
    Placed,
    /// The command is proposed and may not be applied.
    Proposed,
}

type Hook = Arc<dyn Fn(CommitPoint) + Send + Sync>;

#[derive(Default)]
struct Hooks(Mutex<Option<Hook>>);

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hooks")
    }
}

/// Leader-local state of concurrent commits; none of it is replicated.
#[derive(Debug, Default)]
pub(super) struct Concurrent {
    turns: CellTurns,
    hooks: Hooks,
}

#[derive(Debug, Default)]
struct Turns {
    next: u64,
    /// Cells a write holds.
    held: BTreeSet<DeltaCell>,
    /// Writes waiting, by the order they asked in, each with its cells in order.
    waiting: BTreeMap<u64, Vec<DeltaCell>>,
}

/// One write at a time per cell. A write takes all its cells at once, so two never wait for each
/// other, and one that asked later does not overtake one that wants any of the same cells. A
/// holder may wait for the write lock; nobody holding the write lock waits for a turn.
#[derive(Debug, Default)]
struct CellTurns {
    state: Mutex<Turns>,
    ended: Condvar,
}

/// The cells of one write, until dropped.
struct Turn<'a> {
    turns: &'a CellTurns,
    cells: Vec<DeltaCell>,
}

impl CellTurns {
    fn lock(&self) -> MutexGuard<'_, Turns> {
        // Nothing panics while holding it, and a turn must end while unwinding.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for every cell of `cells`, which are in ascending order.
    fn take(&self, cells: Vec<DeltaCell>) -> Turn<'_> {
        let mut state = self.lock();
        let ticket = state.next;
        state.next += 1;
        state.waiting.insert(ticket, cells);
        loop {
            let mine = &state.waiting[&ticket];
            let blocked = mine.iter().any(|c| state.held.contains(c))
                || state
                    .waiting
                    .range(..ticket)
                    .any(|(_, other)| other.iter().any(|c| mine.binary_search(c).is_ok()));
            if !blocked {
                break;
            }
            state = self
                .ended
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        let cells = state.waiting.remove(&ticket).unwrap_or_default();
        state.held.extend(cells.iter().cloned());
        Turn { turns: self, cells }
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let mut state = self.turns.lock();
        for c in &self.cells {
            state.held.remove(c);
        }
        drop(state);
        self.turns.ended.notify_all();
    }
}

/// The bytes a write puts in one cell.
struct Part<'a> {
    cell: u64,
    /// Where in the cell they start.
    within: usize,
    bytes: &'a [u8],
}

/// `data` at `offset`, cut at the cell boundaries, in order.
fn split(grid: u64, offset: u64, data: &[u8]) -> Vec<Part<'_>> {
    let mut parts = Vec::new();
    let (mut pos, mut rest) = (offset, data);
    while !rest.is_empty() {
        let cell = pos - pos % grid;
        let within = (pos - cell) as usize;
        let n = rest.len().min(grid as usize - within);
        let (bytes, tail) = rest.split_at(n);
        parts.push(Part {
            cell,
            within,
            bytes,
        });
        pos += n as u64;
        rest = tail;
    }
    parts
}

/// Whether no part needs anything its cell holds in `c`: the cell has no extent, or the part
/// covers all of it. (A cell without an extent has no deltas either.)
fn replaces(c: &Catalog, target: &Target, parts: &[Part]) -> Result<bool, NativeError> {
    for p in parts {
        if let Some(base) = target.extent_at(c, p.cell)? {
            if p.within > 0 || p.bytes.len() < base.len() {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

impl Target<'_> {
    fn delta_cell(&self, cell: u64) -> DeltaCell {
        let target = match self {
            Target::Volume(id) => DeltaTarget::Volume { id: id.to_string() },
            Target::File { fs, ino, .. } => DeltaTarget::File {
                fs: fs.to_string(),
                ino: *ino,
            },
        };
        DeltaCell { target, cell }
    }
}

impl NativeEngine {
    #[doc(hidden)]
    pub fn set_commit_hook(&self, hook: Option<Arc<dyn Fn(CommitPoint) + Send + Sync>>) {
        if let Ok(mut h) = self.concurrent.hooks.0.lock() {
            *h = hook;
        }
    }

    fn commit_point(&self, point: CommitPoint) {
        let hook = self.concurrent.hooks.0.lock().ok().and_then(|h| h.clone());
        if let Some(hook) = hook {
            hook(point);
        }
    }

    /// Writes `data` at `offset` of `target` if every cell it touches is new or wholly replaced,
    /// holding the write lock only to propose. `Ok(false)`: nothing was committed, and the
    /// locked path takes the write.
    pub(super) fn write_concurrent(
        &self,
        target: &Target,
        offset: u64,
        data: &[u8],
        in_bounds: impl Fn(&Catalog) -> Result<(), NativeError>,
    ) -> Result<bool, NativeError> {
        // The term to allocate and propose in. On a follower this fails as "not the leader".
        let fence = self.term_fence()?;
        let grid = self.with_catalog(|c| target.grid(c, self.cfg.extent_bytes))??;
        let parts = split(grid, offset, data);
        let check = |c: &Catalog| -> Result<bool, NativeError> {
            in_bounds(c)?;
            replaces(c, target, &parts)
        };
        // Looked at before waiting, so that a partial overwrite does not wait for a turn.
        if !self.with_catalog(check)?? {
            return Ok(false);
        }
        let cells = parts.iter().map(|p| target.delta_cell(p.cell)).collect();
        let _turn = self.concurrent.turns.take(cells);
        // The write that had the turn may have given a cell an extent.
        if !self.with_catalog(check)?? {
            return Ok(false);
        }
        let contents: Vec<(u64, Cow<[u8]>)> = parts
            .iter()
            .map(|p| {
                let content: Cow<[u8]> = if p.within == 0 {
                    p.bytes.into()
                } else {
                    // A new cell: what comes before the bytes is a hole, which reads as zeros.
                    let mut buf = vec![0u8; p.within + p.bytes.len()];
                    buf[p.within..].copy_from_slice(p.bytes);
                    buf.into()
                };
                (p.cell, content)
            })
            .collect();
        let mut dedup = true;
        loop {
            let alloc = self.lease(fence)?;
            let (placed, shared) = self.place_cells(&contents, fence, &alloc, dedup)?;
            self.commit_point(CommitPoint::Placed);
            let bytes: usize = placed.iter().map(ExtentRef::len).sum();
            match self.commit_if(target, placed, shared, fence, &check, &alloc) {
                Ok(true) => {
                    self.telemetry.record_write(bytes);
                    self.telemetry.concurrent_commit();
                    return Ok(true);
                }
                Ok(false) => {
                    self.telemetry.concurrent_conflict();
                    return Ok(false);
                }
                // An extent to be shared was freed meanwhile: place the data afresh.
                Err(e) if dedup && is_stale_share(&e) => dedup = false,
                Err(e) => return Err(e),
            }
        }
    }

    /// Proposes the placed extents if `check` passes on the catalog they will apply on, and waits
    /// for them to apply. `Ok(false)`: it did not pass and nothing was proposed.
    fn commit_if(
        &self,
        target: &Target,
        placed: Vec<ExtentRef>,
        shared: Vec<ExtentId>,
        fence: u64,
        check: impl Fn(&Catalog) -> Result<bool, NativeError>,
        alloc: &inflight::Lease<'_>,
    ) -> Result<bool, NativeError> {
        let command = Self::cells_command(target, placed, shared);
        let mut passed = Ok(false);
        let proposed: Result<Proposed, NativeError> = {
            let _write = self
                .write_lock
                .lock()
                .map_err(|_| NativeError::Poisoned("write"))?;
            self.commit_with(fence, |c| {
                passed = check(c);
                matches!(passed, Ok(true)).then_some(command)
            })
        };
        let proposed = proposed.inspect_err(|e| alloc.settle(e))?;
        if !passed? {
            return Ok(false);
        }
        self.commit_point(CommitPoint::Proposed);
        self.await_commit(proposed)
            .inspect_err(|e| alloc.settle(e))?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
