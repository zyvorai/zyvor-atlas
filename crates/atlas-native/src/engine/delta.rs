// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! The engine side of delta writes (`docs/NATIVE_DELTA_WRITES.md`): reading a cell with its
//! deltas laid over it, folding them back into the base, keeping snapshots clean of them, and
//! waiting before dead extents are reclaimed. All of this runs whether or not delta writes are
//! enabled, because deltas written earlier stay until they are folded.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use super::{
    combiner::{Combiner, Outcome},
    now_ns, Meta, NativeEngine, NativeError, Target, READ_PARALLELISM,
};
use crate::{
    metadata::{
        Catalog, DeltaCell, DeltaItem, DeltaTarget, ExtentId, ExtentRef, MetaCommand, MetaError,
    },
    namespace::InodeKind,
    raft::{RaftError, CAP_DELTA},
};

/// Times a delta write is tried again after it certainly did not apply (a new term, a command
/// rejected under it) before the cell is rewritten instead.
const DELTA_ATTEMPTS: usize = 3;
/// Times one cell is looked up again after a read of it failed because it changed.
const CELL_RETRIES: usize = 8;
/// Dirty cells one maintenance pass looks at when choosing what to fold.
const FOLD_SCAN: usize = 4096;
/// Extents one batched reclaim command frees.
const RECLAIM_BATCH: usize = 1024;
/// Rounds of folding (under the write lock) and of retrying a snapshot before giving up.
const FOLD_ROUNDS: usize = 4;

/// `metadata.delta_writes`. The defaults are starting points, not tuned values.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeltaConfig {
    /// Store small partial overwrites as deltas. Off by default. Deltas written while it was on
    /// are still read and folded after it is turned off.
    pub enabled: bool,
    /// Largest write stored as a delta.
    pub max_bytes: usize,
    /// Deltas one cell may hold; at the limit a write rewrites the cell, which folds them.
    pub max_deltas_per_cell: u32,
    /// Deltas pending in one metadata group.
    pub max_pending_deltas: u64,
    /// Device bytes (summed over replicas) that pending deltas and dead, not yet reclaimed
    /// extents may take together, per metadata group.
    pub max_pending_bytes: u64,
    /// Part of `max_pending_bytes` delta writes leave free for folding, which needs room for a
    /// new base while the old one and the deltas are not reclaimed yet.
    pub fold_headroom_bytes: u64,
    /// A cell with this many deltas is folded by the next pass that reaches it.
    pub fold_after_deltas: u32,
    /// A cell dirty for this long is folded too.
    pub fold_after_ms: u64,
    /// Time between folding passes.
    pub fold_interval_ms: u64,
    /// Cells one pass folds at most: with `fold_interval_ms`, the bound on background folding.
    pub fold_cells_per_pass: usize,
    /// How long an extent stays dead before it is reclaimed, so a read that looked it up just
    /// before it died does not find its space reused.
    pub gc_grace_ms: u64,
}

impl Default for DeltaConfig {
    fn default() -> Self {
        // The `delta-writes-test` feature runs the existing tests with delta writes on; they
        // expect dead extents to be reclaimed at once.
        let test = cfg!(feature = "delta-writes-test");
        Self {
            enabled: test,
            max_bytes: 128 << 10,
            max_deltas_per_cell: 32,
            max_pending_deltas: 65_536,
            max_pending_bytes: 2 << 30,
            fold_headroom_bytes: 512 << 20,
            fold_after_deltas: 8,
            fold_after_ms: 5_000,
            fold_interval_ms: 200,
            fold_cells_per_pass: 8,
            gc_grace_ms: if test { 0 } else { 10_000 },
        }
    }
}

impl DeltaConfig {
    pub fn validate(&self) -> Result<(), NativeError> {
        let invalid = |m: &str| Err(NativeError::Invalid(format!("delta_writes: {m}")));
        if self.max_bytes == 0 {
            return invalid("max_bytes must be > 0");
        }
        if self.fold_headroom_bytes >= self.max_pending_bytes {
            return invalid("fold_headroom_bytes must be below max_pending_bytes");
        }
        if self.fold_interval_ms == 0 || self.fold_cells_per_pass == 0 {
            return invalid("fold_interval_ms and fold_cells_per_pass must be > 0");
        }
        if self.fold_after_deltas == 0 {
            return invalid("fold_after_deltas must be > 0");
        }
        Ok(())
    }
}

/// What must hold no deltas before a snapshot of it is taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Scope {
    Volume(String),
    Fs(String),
}

impl Scope {
    fn cells(&self, c: &Catalog) -> Vec<DeltaCell> {
        match self {
            Scope::Volume(v) => c.dirty_volume_cells(v).cloned().collect(),
            Scope::Fs(f) => c.dirty_fs_cells(f).cloned().collect(),
        }
    }
}

/// A grid cell as one look at the catalog saw it: its base extent and the deltas on it, both
/// carrying the offsets they apply at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CellView {
    /// `logical_offset` is the cell's.
    pub base: ExtentRef,
    /// `(offset within the cell, extent)`, oldest first.
    pub deltas: Vec<(u64, ExtentRef)>,
}

#[derive(Debug, Default)]
struct DeadSince {
    term: u64,
    at: HashMap<ExtentId, Instant>,
}

#[derive(Debug, Default)]
struct FoldQueue {
    /// Where the next pass starts looking.
    cursor: Option<DeltaCell>,
    /// When this leader first saw each cell dirty.
    since: HashMap<DeltaCell, Instant>,
}

/// A step of a delta write, for tests that act between steps.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaPoint {
    /// The bytes are on the data nodes; nothing is proposed.
    Replicated,
    /// A batch is about to be proposed.
    BeforePropose,
    /// The batch is proposed and may not be applied.
    Proposed,
}

type Hook = Arc<dyn Fn(DeltaPoint) + Send + Sync>;

#[derive(Default)]
struct Hooks(Mutex<Option<Hook>>);

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hooks")
    }
}

/// Where a write that can be a delta goes.
struct Plan {
    key: DeltaCell,
    at: u64,
}

/// Where `len` bytes at `offset` of `target` can be stored as a delta: inside one cell, over part
/// of its base extent, within the volume or file. `None`: the ordinary path takes the write (and
/// reports what is wrong with it, if anything is).
fn plan(c: &Catalog, target: &Target, cluster: usize, offset: u64, len: u64) -> Option<Plan> {
    let grid = target.grid(c, cluster).ok()?;
    let cell = offset - offset % grid;
    let at = offset - cell;
    let end = offset.checked_add(len).filter(|_| at + len <= grid)?;
    let (target, base, size) = match target {
        Target::Volume(id) => {
            let v = c.volumes.get(*id)?;
            let target = DeltaTarget::Volume { id: id.to_string() };
            (target, v.extents.get(&cell)?.clone(), v.size_bytes)
        }
        Target::File {
            fs,
            ino,
            replica: false,
        } => {
            let f = c.filesystem(fs).ok().filter(|f| f.replica.is_none())?;
            let inode = f.inode(*ino).ok()?;
            let InodeKind::File { size, extents, .. } = &inode.kind else {
                return None;
            };
            let target = DeltaTarget::File {
                fs: fs.to_string(),
                ino: *ino,
            };
            (target, extents.get(&cell)?.clone(), *size)
        }
        Target::File { replica: true, .. } => return None,
    };
    let base_len = c.extents.get(&base)?.extent.len() as u64;
    let whole = at == 0 && len >= base_len;
    (!whole && at + len <= base_len && end <= size).then_some(Plan {
        key: DeltaCell { target, cell },
        at,
    })
}

/// Room under the caps held by a delta that is not applied yet.
struct Room<'a> {
    state: &'a DeltaState,
    bytes: u64,
}

impl Drop for Room<'_> {
    fn drop(&mut self) {
        self.state
            .inflight_bytes
            .fetch_sub(self.bytes, Ordering::Relaxed);
        self.state.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Leader-local state of delta writes; none of it is replicated.
#[derive(Debug, Default)]
pub(super) struct DeltaState {
    /// The last term `leader_ready` succeeded for.
    ready_term: AtomicU64,
    pub(super) combiner: Combiner,
    /// Deltas being written, and their device bytes.
    inflight: AtomicU64,
    inflight_bytes: AtomicU64,
    hooks: Hooks,
    /// Targets taking no new deltas while a snapshot of them is folded and taken.
    blocked: Mutex<Vec<Scope>>,
    dead: Mutex<DeadSince>,
    /// One reclaim pass at a time, so two never propose the same extent.
    pub(super) reclaiming: Mutex<()>,
    folds: Mutex<FoldQueue>,
}

/// Keeps a [`Scope`] blocked until dropped.
pub(super) struct Block<'a> {
    engine: &'a NativeEngine,
    scope: Scope,
}

impl Drop for Block<'_> {
    fn drop(&mut self) {
        if let Ok(mut b) = self.engine.delta.blocked.lock() {
            if let Some(i) = b.iter().position(|s| *s == self.scope) {
                b.swap_remove(i);
            }
        }
    }
}

pub(super) fn target_of<'a>(t: &'a DeltaTarget) -> Target<'a> {
    match t {
        DeltaTarget::Volume { id } => Target::Volume(id),
        DeltaTarget::File { fs, ino } => Target::File {
            fs,
            ino: *ino,
            replica: false,
        },
    }
}

/// Whether the catalog refused a command only because deltas have to be folded first.
fn is_busy(e: &NativeError) -> bool {
    matches!(
        e,
        NativeError::Metadata(MetaError::Busy(_))
            | NativeError::Raft(RaftError::Rejected(MetaError::Busy(_)))
    )
}

/// Whether the catalog rejected a command (so it certainly did not apply).
pub(super) fn is_rejected(e: &NativeError) -> bool {
    matches!(
        e,
        NativeError::Metadata(_) | NativeError::Raft(RaftError::Rejected(_))
    )
}

impl NativeEngine {
    /// The term to allocate and propose in: `leader_ready` once per term, so that every earlier
    /// term's allocations are applied here before this one hands out space.
    pub(super) fn term_fence(&self) -> Result<u64, NativeError> {
        let Meta::Raft { server, timeout } = &self.meta else {
            return Ok(0);
        };
        if let Some(term) = server.leader_term()? {
            if term == self.delta.ready_term.load(Ordering::Acquire) {
                return Ok(term);
            }
        }
        let term = server.leader_ready(*timeout)?;
        self.delta.ready_term.store(term, Ordering::Release);
        Ok(term)
    }

    /// Whether every metadata node this leader replicates to decodes the delta commands. Older
    /// builds cannot decode them and drop the unknown fields of a catalog they are sent, so no
    /// delta may be written before this is true.
    pub fn delta_supported(&self) -> Result<bool, NativeError> {
        Ok(match &self.meta {
            Meta::Local { .. } => true,
            Meta::Raft { server, .. } => server.peers_support(CAP_DELTA)?,
        })
    }

    pub(super) fn block(&self, scope: Scope) -> Result<Block<'_>, NativeError> {
        self.delta
            .blocked
            .lock()
            .map_err(|_| NativeError::Poisoned("delta"))?
            .push(scope.clone());
        Ok(Block {
            engine: self,
            scope,
        })
    }

    pub(super) fn is_blocked(&self, target: &DeltaTarget) -> bool {
        self.delta.blocked.lock().map_or(true, |b| {
            b.iter().any(|s| match (s, target) {
                (Scope::Volume(v), DeltaTarget::Volume { id }) => v == id,
                (Scope::Fs(f), DeltaTarget::File { fs, .. }) => f == fs,
                _ => false,
            })
        })
    }

    #[doc(hidden)]
    pub fn set_delta_hook(&self, hook: Option<Arc<dyn Fn(DeltaPoint) + Send + Sync>>) {
        if let Ok(mut h) = self.delta.hooks.0.lock() {
            *h = hook;
        }
    }

    pub(super) fn delta_point(&self, point: DeltaPoint) {
        let hook = self.delta.hooks.0.lock().ok().and_then(|h| h.clone());
        if let Some(hook) = hook {
            hook(point);
        }
    }

    /// Takes room under the caps for one more delta on `key` costing `bytes` of device space.
    fn room(&self, key: &DeltaCell, bytes: u64) -> Result<Option<Room<'_>>, NativeError> {
        let cfg = &self.cfg.delta_writes;
        let state = &self.delta;
        let writing = state.inflight.fetch_add(1, Ordering::Relaxed) + 1;
        let writing_bytes = state.inflight_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        let room = Room { state, bytes };
        let fits = self.with_catalog(|c| {
            let t = c.delta_totals;
            t.pending_bytes + t.dead_bytes + writing_bytes
                <= cfg.max_pending_bytes - cfg.fold_headroom_bytes
                && t.pending + writing <= cfg.max_pending_deltas
                && c.dirty.get(key).copied().unwrap_or(0) < cfg.max_deltas_per_cell
        })?;
        Ok(fits.then_some(room))
    }

    /// Stores `data` as a delta if it can be one: its bytes go to the data nodes without the
    /// write lock, then join the next delta command. `Ok(false)`: nothing was written, and the
    /// ordinary path takes the write.
    pub(super) fn try_delta(
        &self,
        target: &Target,
        offset: u64,
        data: &[u8],
    ) -> Result<bool, NativeError> {
        let cfg = &self.cfg.delta_writes;
        let replica = matches!(target, Target::File { replica: true, .. });
        if !cfg.enabled || replica || data.is_empty() || data.len() > cfg.max_bytes {
            return Ok(false);
        }
        let cost = data.len() as u64 * self.cfg.placement.replicas as u64;
        for _ in 0..DELTA_ATTEMPTS {
            // Before the catalog is read or space is taken.
            let fence = self.term_fence()?;
            if !self.delta_supported()? {
                return Ok(false);
            }
            let cluster = self.cfg.extent_bytes;
            let Some(plan) =
                self.with_catalog(|c| plan(c, target, cluster, offset, data.len() as u64))?
            else {
                return Ok(false);
            };
            if self.is_blocked(&plan.key.target) {
                break;
            }
            let Some(_room) = self.room(&plan.key, cost)? else {
                break;
            };
            let alloc = self.lease(fence)?;
            let Some(extent) = self.place_delta(data, fence, &alloc)? else {
                break;
            };
            self.delta_point(DeltaPoint::Replicated);
            let item = DeltaItem {
                target: plan.key.target,
                cell: plan.key.cell,
                at: plan.at,
                extent,
                now_ns: now_ns(),
            };
            match self.submit_delta(fence, item, alloc.detach())? {
                Outcome::Applied => {
                    self.telemetry.delta_written();
                    self.telemetry.record_write(data.len());
                    return Ok(true);
                }
                Outcome::Unknown(why) => return Err(NativeError::Unknown(why)),
                Outcome::Rejected => {}
            }
        }
        self.telemetry.delta_fallback();
        Ok(false)
    }

    /// Reads a cell: the base with its deltas laid over it, from one look at the catalog. If a
    /// read fails because the cell changed meanwhile (folded, rewritten and reclaimed), the
    /// whole cell is looked up again. Returns the view the bytes came from.
    pub(super) fn read_view(
        &self,
        mut view: CellView,
        source: Option<&Target>,
    ) -> Result<(CellView, Vec<u8>), NativeError> {
        let mut tries = 0;
        loop {
            match self.read_view_once(&view) {
                Err(NativeError::Checksum(id)) if tries < CELL_RETRIES => {
                    let Some(source) = source else {
                        return Err(NativeError::Checksum(id));
                    };
                    let now =
                        self.with_catalog(|c| source.cell_view(c, view.base.logical_offset))??;
                    match now {
                        Some(now) if now != view => view = now,
                        _ => return Err(NativeError::Checksum(id)),
                    }
                    tries += 1;
                }
                Err(e) => return Err(e),
                Ok(bytes) => return Ok((view, bytes)),
            }
        }
    }

    fn read_view_once(&self, view: &CellView) -> Result<Vec<u8>, NativeError> {
        let mut out = self.read_extent(&view.base)?;
        for window in view.deltas.chunks(READ_PARALLELISM) {
            let bufs: Vec<Result<Vec<u8>, NativeError>> = if window.len() == 1 {
                vec![self.read_extent(&window[0].1)]
            } else {
                std::thread::scope(|s| {
                    let handles: Vec<_> = window
                        .iter()
                        .map(|(_, e)| s.spawn(|| self.read_extent(e)))
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join()
                                .unwrap_or(Err(NativeError::Poisoned("delta reader")))
                        })
                        .collect()
                })
            };
            // In list order: a later delta wins where two overlap.
            for ((at, e), buf) in window.iter().zip(bufs) {
                let buf = buf?;
                let at = *at as usize;
                out.get_mut(at..at + buf.len())
                    .ok_or_else(|| {
                        NativeError::Invalid(format!("delta {} lies outside its cell", e.id))
                    })?
                    .copy_from_slice(&buf);
            }
        }
        Ok(out)
    }

    /// Folds one cell's deltas into a new base. `Ok(false)` if there was nothing to fold or the
    /// cell changed before the fold could commit. Deltas appended meanwhile stay on the new
    /// base. Needs no write lock: the commit is a compare-and-swap.
    pub(super) fn fold_cell(&self, key: &DeltaCell) -> Result<bool, NativeError> {
        let fence = self.term_fence()?;
        let target = target_of(&key.target);
        // A cell or file that is gone has nothing to fold.
        let view = self.with_catalog(|c| target.cell_view(c, key.cell).ok().flatten())?;
        let Some(view) = view.filter(|v| !v.deltas.is_empty()) else {
            return Ok(false);
        };
        let (view, content) = self.read_view(view, Some(&target))?;
        if view.deltas.is_empty() {
            return Ok(false);
        }
        let alloc = self.lease(fence)?;
        let mut extent = self.place_extent(&content, fence, &alloc)?;
        extent.logical_offset = key.cell;
        let cmd = MetaCommand::FoldCell {
            cell: key.clone(),
            base: view.base.id.clone(),
            consumed: view.deltas.iter().map(|(_, e)| e.id.clone()).collect(),
            extent,
        };
        match self
            .commit(cmd, Some(fence))
            .inspect_err(|e| alloc.settle(e))
        {
            Ok(()) => {
                self.telemetry.delta_folded();
                Ok(true)
            }
            Err(e) if is_rejected(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn fold_scope(&self, scope: &Scope) -> Result<(), NativeError> {
        for cell in self.with_catalog(|c| scope.cells(c))? {
            self.fold_cell(&cell)?;
        }
        Ok(())
    }

    /// Runs `commit` (a snapshot of `scope`) once `scope` holds no deltas: folds what it can
    /// without the write lock, stops new deltas on the scope, then folds the rest and commits
    /// under the lock, so the snapshot, its clones and its replicas never see a delta.
    pub(super) fn commit_folded(
        &self,
        scope: Scope,
        commit: impl Fn(Option<u64>) -> Result<(), NativeError>,
    ) -> Result<(), NativeError> {
        let dirty = || self.with_catalog(|c| !scope.cells(c).is_empty());
        if !self.cfg.delta_writes.enabled && !dirty()? {
            match commit(None) {
                // A delta this replica had not applied yet: fold, below.
                Err(e) if is_busy(&e) => {}
                r => return r,
            }
        }
        let mut last = None;
        for _ in 0..FOLD_ROUNDS {
            self.fold_scope(&scope)?;
            let _block = self.block(scope.clone())?;
            let _write = self
                .write_lock
                .lock()
                .map_err(|_| NativeError::Poisoned("write"))?;
            // Waits for every delta batch proposed before the lock was taken; none can be
            // proposed while it is held.
            let fence = self.write_fence()?;
            for _ in 0..FOLD_ROUNDS {
                if !dirty()? {
                    break;
                }
                self.fold_scope(&scope)?;
            }
            match commit(Some(fence)) {
                Err(e) if is_busy(&e) => last = Some(e),
                r => return r,
            }
        }
        Err(last.unwrap_or(NativeError::Poisoned("fold")))
    }

    /// Of the dead extents `candidates` (id, and whether the grace period applies to it), those
    /// that may be reclaimed now. The clock starts when this leader first sees one dead.
    pub(super) fn ripe(
        &self,
        term: u64,
        candidates: Vec<(ExtentId, bool)>,
    ) -> Result<Vec<ExtentId>, NativeError> {
        let grace = Duration::from_millis(self.cfg.delta_writes.gc_grace_ms);
        let mut dead = self
            .delta
            .dead
            .lock()
            .map_err(|_| NativeError::Poisoned("delta"))?;
        if dead.term != term {
            *dead = DeadSince {
                term,
                at: HashMap::new(),
            };
        }
        let now = Instant::now();
        let mut seen = HashMap::new();
        let mut ripe = Vec::new();
        for (id, graced) in candidates {
            if !graced || grace.is_zero() {
                ripe.push(id);
                continue;
            }
            let at = dead.at.get(&id).copied().unwrap_or(now);
            if now.duration_since(at) >= grace {
                ripe.push(id.clone());
            }
            seen.insert(id, at);
        }
        dead.at = seen;
        Ok(ripe)
    }

    /// Commits the reclaim of `ids`: in batches once delta writes are in use and every metadata
    /// node decodes the batched command, one at a time otherwise. Calls `done` for each batch.
    pub(super) fn reclaim(
        &self,
        ids: &[ExtentId],
        fence: u64,
        mut done: impl FnMut(&[ExtentId]),
    ) -> Result<(), NativeError> {
        let in_use = self.cfg.delta_writes.enabled || self.with_catalog(|c| !c.dirty.is_empty())?;
        if in_use && self.delta_supported()? {
            for batch in ids.chunks(RECLAIM_BATCH) {
                let objects = self.objects_of(batch)?;
                let extent_ids = batch.to_vec();
                self.commit(MetaCommand::ReclaimExtents { extent_ids }, Some(fence))?;
                self.delete_objects(&objects);
                done(batch);
            }
            return Ok(());
        }
        for id in ids {
            let objects = self.objects_of(std::slice::from_ref(id))?;
            let extent_id = id.clone();
            self.commit(MetaCommand::MarkExtentReclaimed { extent_id }, None)?;
            self.delete_objects(&objects);
            done(std::slice::from_ref(id));
        }
        Ok(())
    }

    fn objects_of(&self, ids: &[ExtentId]) -> Result<Vec<String>, NativeError> {
        self.with_catalog(|c| {
            ids.iter()
                .filter_map(|id| c.extents.get(id).and_then(|e| e.extent.object.clone()))
                .collect()
        })
    }

    /// A failed delete leaves an orphan for the next tiering pass's sweep.
    fn delete_objects(&self, keys: &[String]) {
        if let Some(store) = &self.cfg.objects {
            for key in keys {
                let _ = store.delete(key);
            }
        }
    }

    /// Whether pending deltas and dead extents have used up half of what delta writes may.
    fn delta_pressure(&self, c: &Catalog) -> bool {
        let t = c.delta_totals;
        let room =
            self.cfg.delta_writes.max_pending_bytes - self.cfg.delta_writes.fold_headroom_bytes;
        t.pending_bytes + t.dead_bytes >= room / 2
            || t.pending >= self.cfg.delta_writes.max_pending_deltas / 2
    }

    /// Up to `fold_cells_per_pass` dirty cells due for folding, continuing round-robin where the
    /// last pass stopped.
    fn due_folds(&self) -> Result<Vec<DeltaCell>, NativeError> {
        let cfg = &self.cfg.delta_writes;
        let mut q = self
            .delta
            .folds
            .lock()
            .map_err(|_| NativeError::Poisoned("delta"))?;
        let now = Instant::now();
        let age = Duration::from_millis(cfg.fold_after_ms);
        let due = self.with_catalog(|c| {
            // Turned off, nothing keeps deltas around: fold them all.
            let all = !cfg.enabled || self.delta_pressure(c);
            let after = q.cursor.clone();
            let ahead = c.dirty.iter().filter(|(k, _)| match &after {
                Some(a) => *k > a,
                None => true,
            });
            let wrapped = c.dirty.iter().filter(|(k, _)| match &after {
                Some(a) => *k <= a,
                None => false,
            });
            let mut due = Vec::new();
            for (key, count) in ahead.chain(wrapped).take(FOLD_SCAN) {
                q.cursor = Some(key.clone());
                let since = *q.since.entry(key.clone()).or_insert(now);
                if all || *count >= cfg.fold_after_deltas || now.duration_since(since) >= age {
                    due.push(key.clone());
                    if due.len() >= cfg.fold_cells_per_pass {
                        break;
                    }
                }
            }
            q.since.retain(|k, _| c.dirty.contains_key(k));
            due
        })?;
        for key in &due {
            q.since.remove(key);
        }
        Ok(due)
    }

    /// One background pass on the leader: folds the cells that are due, at most
    /// `fold_cells_per_pass` of them, and reclaims dead extents early when they and the pending
    /// deltas have used up half the room delta writes have. Returns the cells folded. Does
    /// nothing on a follower, or with no delta pending and delta writes off.
    pub fn delta_maintenance_once(&self) -> Result<usize, NativeError> {
        if let Meta::Raft { server, .. } = &self.meta {
            if server.leader_term()?.is_none() {
                return Ok(0);
            }
        }
        if !self.cfg.delta_writes.enabled && self.with_catalog(|c| c.dirty.is_empty())? {
            return Ok(0);
        }
        let mut folded = 0;
        for key in self.due_folds()? {
            folded += usize::from(self.fold_cell(&key)?);
        }
        if self.with_catalog(|c| c.delta_totals.dead_bytes > 0 && self.delta_pressure(c))? {
            self.gc_once()?;
        }
        Ok(folded)
    }

    /// Folds every pending delta of this group (for tests and for turning delta writes off).
    pub fn fold_all_deltas(&self) -> Result<usize, NativeError> {
        let mut folded = 0;
        for key in self.with_catalog(|c| c.dirty.keys().cloned().collect::<Vec<_>>())? {
            folded += usize::from(self.fold_cell(&key)?);
        }
        Ok(folded)
    }

    /// Cells holding deltas, and the deltas pending in them.
    pub fn pending_deltas(&self) -> Result<(usize, u64), NativeError> {
        self.with_catalog(|c| (c.dirty.len(), c.delta_totals.pending))
    }
}

#[cfg(test)]
mod tests;
