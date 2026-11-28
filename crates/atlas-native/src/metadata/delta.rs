// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Delta writes in the catalog (`docs/NATIVE_DELTA_WRITES.md`): a small overwrite is stored as an
//! extra extent holding only the written bytes, listed beside the cell's base extent. Reads lay
//! the deltas over the base in list order; a fold replaces the base with that content and removes
//! the deltas it consumed.
//!
//! Every command here checks all of its input before it changes anything, and reads only the
//! catalog, never a node's configuration, so every replica applies it the same way.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::RangeInclusive,
};

use serde::{Deserialize, Serialize};

use super::{Catalog, ExtentId, ExtentRef, MetaError, VolumeId};
use crate::namespace::{FsId, InodeKind};

/// What a delta cell belongs to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DeltaTarget {
    Volume { id: VolumeId },
    File { fs: FsId, ino: u64 },
}

/// One grid cell of a volume or file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeltaCell {
    pub target: DeltaTarget,
    /// Logical offset of the cell (its key in the extent map).
    pub cell: u64,
}

/// A delta listed on a cell: an extent laid over the base at byte `at` of the cell.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeltaRef {
    pub id: ExtentId,
    pub at: u64,
}

/// One write of an append batch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeltaItem {
    pub target: DeltaTarget,
    pub cell: u64,
    pub at: u64,
    /// The bytes written, as a new extent with [`ExtentRef::delta`] set.
    pub extent: ExtentRef,
    /// The writer's clock, stamped on a file's mtime and ctime.
    #[serde(default)]
    pub now_ns: i64,
}

/// One compare-and-swap fold of a batch ([`super::MetaCommand::FoldCells`]): the fields of a
/// [`super::MetaCommand::FoldCell`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CellFold {
    pub cell: DeltaCell,
    pub base: ExtentId,
    pub consumed: Vec<ExtentId>,
    pub extent: ExtentRef,
}

/// Cell lists, keyed by cell offset, as volume and file records hold them.
pub type DeltaMap = BTreeMap<u64, Vec<DeltaRef>>;

/// Device bytes (summed over replicas) held by deltas and by dead extents. Derived from the
/// extent table, so not stored.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeltaTotals {
    /// Delta extents listed on some cell.
    pub pending: u64,
    pub pending_bytes: u64,
    /// Extents nothing references that are not reclaimed yet (of any kind).
    pub dead_bytes: u64,
}

/// The dirty-cell index as a list: JSON map keys must be strings.
pub(crate) mod dirty_cells {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serializer};

    use super::DeltaCell;

    pub fn serialize<S: Serializer>(m: &BTreeMap<DeltaCell, u32>, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(m.iter())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<DeltaCell, u32>, D::Error> {
        Ok(Vec::<(DeltaCell, u32)>::deserialize(d)?
            .into_iter()
            .collect())
    }
}

/// Device bytes an extent takes, over all its replicas or shards.
pub(crate) fn device_cost(e: &ExtentRef) -> u64 {
    e.stored_len() * e.replicas.len() as u64
}

fn volume_range(id: &str) -> RangeInclusive<DeltaCell> {
    let at = |cell| DeltaCell {
        target: DeltaTarget::Volume { id: id.to_string() },
        cell,
    };
    at(0)..=at(u64::MAX)
}

fn fs_range(fs: &str, inos: RangeInclusive<u64>) -> RangeInclusive<DeltaCell> {
    let at = |ino, cell| DeltaCell {
        target: DeltaTarget::File {
            fs: fs.to_string(),
            ino,
        },
        cell,
    };
    at(*inos.start(), 0)..=at(*inos.end(), u64::MAX)
}

/// A cell as the catalog has it: its base extent, the end of its volume or file, and its deltas.
pub(crate) struct CellState {
    base: Option<ExtentId>,
    size: u64,
    deltas: Vec<DeltaRef>,
}

impl Catalog {
    pub fn volume_is_dirty(&self, id: &str) -> bool {
        self.dirty.range(volume_range(id)).next().is_some()
    }

    pub fn fs_is_dirty(&self, fs: &str) -> bool {
        self.dirty
            .range(fs_range(fs, 0..=u64::MAX))
            .next()
            .is_some()
    }

    /// The dirty cells of a volume, or of every file of a filesystem.
    pub fn dirty_volume_cells(&self, id: &str) -> impl Iterator<Item = &DeltaCell> {
        self.dirty.range(volume_range(id)).map(|(k, _)| k)
    }

    pub fn dirty_fs_cells(&self, fs: &str) -> impl Iterator<Item = &DeltaCell> {
        self.dirty.range(fs_range(fs, 0..=u64::MAX)).map(|(k, _)| k)
    }

    fn cell_state(&self, target: &DeltaTarget, cell: u64) -> Result<CellState, MetaError> {
        match target {
            DeltaTarget::Volume { id } => {
                let v = self
                    .volumes
                    .get(id)
                    .ok_or_else(|| MetaError::NotFound(id.clone()))?;
                Ok(CellState {
                    base: v.extents.get(&cell).cloned(),
                    size: v.size_bytes,
                    deltas: v.deltas.get(&cell).cloned().unwrap_or_default(),
                })
            }
            DeltaTarget::File { fs, ino } => {
                let inode = self.filesystem(fs)?.inode(*ino)?;
                let InodeKind::File {
                    size,
                    extents,
                    deltas,
                } = &inode.kind
                else {
                    return Err(MetaError::IsDir(format!(
                        "inode {ino} is not a regular file"
                    )));
                };
                Ok(CellState {
                    base: extents.get(&cell).cloned(),
                    size: *size,
                    deltas: deltas.get(&cell).cloned().unwrap_or_default(),
                })
            }
        }
    }

    /// Whether `item` could be appended now: its target is a file or volume that still has a base
    /// extent at the cell, the bytes lie inside both that extent and the volume or file, and the
    /// delta extent is a new, plain replicated one. Changes nothing.
    pub fn check_delta(&self, item: &DeltaItem) -> Result<(), MetaError> {
        let e = &item.extent;
        e.check()?;
        let invalid = |what: &str| Err(MetaError::Invalid(format!("delta extent {} {what}", e.id)));
        if !e.delta {
            return invalid("is not marked as a delta");
        }
        if e.is_empty() || e.replicas.is_empty() {
            return invalid("is empty or has no replica");
        }
        if e.ec.is_some() || e.object.is_some() || e.raw_len != 0 {
            return invalid("must be stored as plain replicas");
        }
        if self.extents.contains_key(&e.id) {
            return invalid("already exists");
        }
        if let DeltaTarget::File { fs, .. } = &item.target {
            if self.filesystem(fs)?.replica.is_some() {
                return Err(MetaError::ReadOnly(format!(
                    "filesystem {fs} is a replica; promote it to change it"
                )));
            }
        }
        let state = self.cell_state(&item.target, item.cell)?;
        let base = state
            .base
            .ok_or_else(|| MetaError::Invalid(format!("cell {} has no extent", item.cell)))?;
        let base_len = self
            .extents
            .get(&base)
            .ok_or_else(|| MetaError::NotFound(base.clone()))?
            .extent
            .len() as u64;
        let end = item.at.checked_add(e.len() as u64);
        if end.is_none_or(|end| end > base_len) {
            return invalid("reaches past the cell's extent");
        }
        let within_size = end
            .and_then(|end| item.cell.checked_add(end))
            .is_some_and(|end| end <= state.size);
        if !within_size {
            return invalid("reaches past the end of its volume or file");
        }
        Ok(())
    }

    /// Appends every item, or none: `volumes` says which kind of target the command may name.
    pub(crate) fn append_deltas(
        &mut self,
        items: &[DeltaItem],
        volumes: bool,
    ) -> Result<(), MetaError> {
        if items.is_empty() {
            return Err(MetaError::Invalid("empty delta batch".into()));
        }
        let mut ids = BTreeSet::new();
        for item in items {
            if matches!(item.target, DeltaTarget::Volume { .. }) != volumes {
                return Err(MetaError::Invalid(
                    "delta batch mixes up volumes and files".into(),
                ));
            }
            if !ids.insert(&item.extent.id) {
                return Err(MetaError::Invalid(format!(
                    "delta extent {} is listed twice",
                    item.extent.id
                )));
            }
            self.check_delta(item)?;
        }
        for item in items {
            let delta = DeltaRef {
                id: item.extent.id.clone(),
                at: item.at,
            };
            match &item.target {
                DeltaTarget::Volume { id } => {
                    self.volumes
                        .get_mut(id)
                        .expect("checked")
                        .deltas
                        .entry(item.cell)
                        .or_default()
                        .push(delta);
                }
                DeltaTarget::File { fs, ino } => {
                    let inode = self.fs_mut(fs)?.inode_mut(*ino)?;
                    inode.touch(item.now_ns);
                    if let InodeKind::File { deltas, .. } = &mut inode.kind {
                        deltas.entry(item.cell).or_default().push(delta);
                    }
                }
            }
            self.add_extent_ref(&item.extent);
            *self
                .dirty
                .entry(DeltaCell {
                    target: item.target.clone(),
                    cell: item.cell,
                })
                .or_default() += 1;
        }
        Ok(())
    }

    /// Whether a fold would apply now: the base is still `base`, `consumed` is still the start
    /// of the cell's delta list, and `extent` is a new extent that can replace the base. Returns
    /// the cell as it is. Changes nothing.
    pub(crate) fn check_fold(
        &self,
        key: &DeltaCell,
        base: &str,
        consumed: &[ExtentId],
        extent: &ExtentRef,
    ) -> Result<CellState, MetaError> {
        extent.check()?;
        let changed = || {
            MetaError::Invalid(format!(
                "cell {} changed before it could be folded",
                key.cell
            ))
        };
        let state = self.cell_state(&key.target, key.cell)?;
        if state.base.as_deref() != Some(base) {
            return Err(changed());
        }
        let prefix = state.deltas.len() >= consumed.len()
            && state.deltas.iter().zip(consumed).all(|(d, c)| d.id == *c);
        if consumed.is_empty() || !prefix {
            return Err(changed());
        }
        let base_len = self
            .extents
            .get(base)
            .ok_or_else(|| MetaError::NotFound(base.to_string()))?
            .extent
            .len();
        if extent.delta || extent.len() != base_len || self.extents.contains_key(&extent.id) {
            return Err(MetaError::Invalid(format!(
                "extent {} cannot replace the {base_len}-byte base of cell {}",
                extent.id, key.cell
            )));
        }
        Ok(state)
    }

    /// Compare-and-swap fold: `extent` becomes the cell's base and the `consumed` deltas go,
    /// provided the base is still `base` and `consumed` is still the start of the cell's list.
    /// Deltas appended since stay on top. The content is unchanged, so neither times nor usage
    /// move.
    pub(crate) fn fold_cell(
        &mut self,
        key: &DeltaCell,
        base: &str,
        consumed: &[ExtentId],
        extent: &ExtentRef,
        gc: &mut Vec<ExtentId>,
    ) -> Result<(), MetaError> {
        let state = self.check_fold(key, base, consumed, extent)?;
        self.apply_fold(state, key, base, consumed, extent, gc)
    }

    /// Several folds in the order listed, each its own compare-and-swap: one whose check fails
    /// against the catalog as the folds before it left it is skipped, the rest apply. Only the
    /// catalog decides, so every replica skips the same ones.
    pub(crate) fn fold_cells(
        &mut self,
        folds: &[CellFold],
        gc: &mut Vec<ExtentId>,
    ) -> Result<(), MetaError> {
        if folds.is_empty() {
            return Err(MetaError::Invalid("empty fold batch".into()));
        }
        for f in folds {
            match self.check_fold(&f.cell, &f.base, &f.consumed, &f.extent) {
                Ok(state) => {
                    self.apply_fold(state, &f.cell, &f.base, &f.consumed, &f.extent, gc)?
                }
                // Not a property of the command: other replicas may well read their store.
                Err(e @ MetaError::Store(_)) => return Err(e),
                Err(_) => {}
            }
        }
        Ok(())
    }

    /// The change a fold makes, once [`Self::check_fold`] has returned `state` for it.
    fn apply_fold(
        &mut self,
        state: CellState,
        key: &DeltaCell,
        base: &str,
        consumed: &[ExtentId],
        extent: &ExtentRef,
        gc: &mut Vec<ExtentId>,
    ) -> Result<(), MetaError> {
        let rest: Vec<DeltaRef> = state.deltas[consumed.len()..].to_vec();
        let remaining = rest.len() as u32;
        let set = |extents: &mut BTreeMap<u64, ExtentId>, deltas: &mut DeltaMap| {
            extents.insert(key.cell, extent.id.clone());
            if rest.is_empty() {
                deltas.remove(&key.cell);
            } else {
                deltas.insert(key.cell, rest);
            }
        };
        match &key.target {
            DeltaTarget::Volume { id } => {
                let v = self.volumes.get_mut(id).expect("checked");
                set(&mut v.extents, &mut v.deltas);
            }
            DeltaTarget::File { fs, ino } => {
                let inode = self.fs_mut(fs)?.inode_mut(*ino)?;
                if let InodeKind::File {
                    extents, deltas, ..
                } = &mut inode.kind
                {
                    set(extents, deltas);
                }
            }
        }
        if remaining == 0 {
            self.dirty.remove(key);
        } else {
            self.dirty.insert(key.clone(), remaining);
        }
        self.add_extent_ref(extent);
        self.dec_ref(base, gc)?;
        for id in consumed {
            self.dec_ref(id, gc)?;
        }
        Ok(())
    }

    /// Forgets the delta list a cell no longer has (its base was replaced, cut or removed).
    pub(crate) fn drop_deltas(
        &mut self,
        target: DeltaTarget,
        cell: u64,
        list: Vec<DeltaRef>,
        gc: &mut Vec<ExtentId>,
    ) -> Result<(), MetaError> {
        self.dirty.remove(&DeltaCell { target, cell });
        for d in list {
            self.dec_ref(&d.id, gc)?;
        }
        Ok(())
    }

    /// Refuses to cut a file inside a cell that has deltas: the writer folds that cell first.
    /// Cells wholly past the new size are simply dropped with their deltas.
    pub(crate) fn check_truncate(
        &self,
        fs: &str,
        ino: u64,
        new_size: u64,
    ) -> Result<(), MetaError> {
        let inode = self.filesystem(fs)?.inode(ino)?;
        let InodeKind::File {
            size,
            extents,
            deltas,
        } = &inode.kind
        else {
            return Ok(());
        };
        if new_size >= *size {
            return Ok(());
        }
        let Some((cell, base)) = extents.range(..new_size).next_back() else {
            return Ok(());
        };
        let base_len = self.extents.get(base).map_or(0, |m| m.extent.len() as u64);
        if cell + base_len > new_size && deltas.contains_key(cell) {
            return Err(MetaError::Busy(format!(
                "inode {ino}: the extent at {cell} has pending deltas"
            )));
        }
        Ok(())
    }

    /// Reclaims every listed dead extent, or none.
    pub(crate) fn reclaim_extents(&mut self, ids: &[ExtentId]) -> Result<(), MetaError> {
        let mut seen = BTreeSet::new();
        for id in ids {
            self.check_reclaim(id)?;
            if !seen.insert(id) {
                return Err(MetaError::Invalid(format!("extent {id} is listed twice")));
            }
        }
        for id in ids {
            self.reclaim_extent(id)?;
        }
        Ok(())
    }

    pub(crate) fn check_reclaim(&self, id: &str) -> Result<(), MetaError> {
        let e = self
            .extents
            .get(id)
            .ok_or_else(|| MetaError::NotFound(id.to_string()))?;
        if e.refs != 0 {
            return Err(MetaError::Invalid(format!(
                "extent {id} still has {} refs",
                e.refs
            )));
        }
        Ok(())
    }

    /// Removes a dead extent (see [`Self::check_reclaim`]) and frees its ranges.
    pub(crate) fn reclaim_extent(&mut self, id: &str) -> Result<(), MetaError> {
        let e = self
            .extents
            .remove(id)
            .ok_or_else(|| MetaError::NotFound(id.to_string()))?;
        self.delta_totals.dead_bytes = self
            .delta_totals
            .dead_bytes
            .saturating_sub(device_cost(&e.extent));
        for r in &e.extent.replicas {
            self.free
                .release(&r.node_id, r.device_index, r.offset, e.extent.stored_len())
                .map_err(MetaError::Invalid)?;
        }
        Ok(())
    }

    /// Counts [`DeltaTotals`] from the extent table (after loading a catalog).
    pub fn fill_delta_totals(&mut self) {
        let mut t = DeltaTotals::default();
        for m in self.extents.values() {
            let cost = device_cost(&m.extent);
            if m.refs == 0 {
                t.dead_bytes += cost;
            } else if m.extent.delta {
                t.pending += 1;
                t.pending_bytes += cost;
            }
        }
        self.delta_totals = t;
    }

    /// Checks that the dirty-cell index and the totals agree with a fresh scan of every record
    /// (for tests).
    #[doc(hidden)]
    pub fn check_delta_invariants(&self) -> Result<(), String> {
        let mut found: BTreeMap<DeltaCell, u32> = BTreeMap::new();
        let mut listed = BTreeSet::new();
        let mut note = |target: DeltaTarget,
                        extents: &BTreeMap<u64, ExtentId>,
                        deltas: &DeltaMap,
                        size: u64|
         -> Result<(), String> {
            for (cell, list) in deltas {
                let key = DeltaCell {
                    target: target.clone(),
                    cell: *cell,
                };
                if list.is_empty() {
                    return Err(format!("{key:?} has an empty delta list"));
                }
                let base = extents
                    .get(cell)
                    .and_then(|id| self.extents.get(id))
                    .ok_or_else(|| format!("{key:?} has deltas but no base"))?;
                for d in list {
                    let m = self
                        .extents
                        .get(&d.id)
                        .ok_or_else(|| format!("{key:?}: delta {} is gone", d.id))?;
                    if !m.extent.delta || m.refs != 1 || !listed.insert(d.id.clone()) {
                        return Err(format!("{key:?}: delta {} is not exclusive", d.id));
                    }
                    let end = d.at + m.extent.len() as u64;
                    if end > base.extent.len() as u64 || cell + end > size {
                        return Err(format!("{key:?}: delta {} out of bounds", d.id));
                    }
                }
                found.insert(key, list.len() as u32);
            }
            Ok(())
        };
        for v in self.volumes.values() {
            note(
                DeltaTarget::Volume { id: v.id.clone() },
                &v.extents,
                &v.deltas,
                v.size_bytes,
            )?;
        }
        for f in self.filesystems.values() {
            for i in f.inodes.scan().map_err(|e| e.to_string())? {
                if let InodeKind::File {
                    size,
                    extents,
                    deltas,
                } = &i.kind
                {
                    let target = DeltaTarget::File {
                        fs: f.id.clone(),
                        ino: i.ino,
                    };
                    note(target, extents, deltas, *size)?;
                }
            }
        }
        for s in self.fs_snapshots.values() {
            for i in s.tree.inodes.scan().map_err(|e| e.to_string())? {
                if matches!(&i.kind, InodeKind::File { deltas, .. } if !deltas.is_empty()) {
                    return Err(format!("snapshot {} holds deltas", s.id));
                }
            }
        }
        if found != self.dirty {
            return Err(format!(
                "dirty index {:?} differs from the records {found:?}",
                self.dirty
            ));
        }
        for (id, m) in self.extents.iter() {
            if m.extent.delta && m.refs > 0 && !listed.contains(id) {
                return Err(format!("live delta {id} is listed nowhere"));
            }
        }
        let mut fresh = self.clone();
        fresh.fill_delta_totals();
        if fresh.delta_totals != self.delta_totals {
            return Err(format!(
                "totals {:?} differ from a fresh count {:?}",
                self.delta_totals, fresh.delta_totals
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
