// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{
    alloc::FreeList,
    ec::EcLayout,
    leases::{LeaseOp, Leases},
    membership::Membership,
    namespace::{FsId, FsMeta, FsOp, FsSnapshotMeta},
    tracked::Tracked,
};

pub type VolumeId = String;
pub type SnapshotId = String;
pub type ExtentId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplicaRef {
    pub node_id: String,
    pub device_index: usize,
    pub offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtentRef {
    pub id: ExtentId,
    pub logical_offset: u64,
    /// Bytes stored on the devices (the compressed payload when `raw_len` is non-zero).
    #[serde(rename = "len")]
    pub payload_len: usize,
    /// Uncompressed length when the payload is LZ4-compressed; 0 = stored as is. Everything below
    /// the engine's read path (devices, scrub, repair, tiering) works on the payload and
    /// `checksum` covers the payload; only logical consumers use [`ExtentRef::len`].
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub raw_len: usize,
    pub checksum: [u8; 32],
    /// SHA-256 of the logical bytes, set when they differ from the payload (compressed extents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<[u8; 32]>,
    /// Full copies; for an erasure-coded extent, shard `i` of `ec` is `replicas[i]`.
    pub replicas: Vec<ReplicaRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ec: Option<EcLayout>,
    /// When the leader wrote it (ms since the Unix epoch; 0 for extents from before this was
    /// recorded). Extents are never rewritten, so this is also when its data last changed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub created_ms: u64,
    /// The object holding the whole extent once it is tiered; it then has no replicas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

fn is_zero_usize(v: &usize) -> bool {
    *v == 0
}

impl ExtentRef {
    /// Logical bytes the extent covers in its volume or file.
    pub fn len(&self) -> usize {
        if self.raw_len > 0 {
            self.raw_len
        } else {
            self.payload_len
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// SHA-256 of the logical bytes (the payload checksum when nothing is compressed).
    pub fn content_sha(&self) -> [u8; 32] {
        self.content.unwrap_or(self.checksum)
    }

    /// Bytes each replica (or shard) takes on its device.
    pub fn stored_len(&self) -> u64 {
        self.ec.as_ref().map_or(self.payload_len, |e| e.shard_len) as u64
    }

    /// Checks an extent about to be installed: an erasure-coded one must match its layout, and
    /// no two of its replicas or shards may share a node.
    pub fn check(&self) -> Result<(), MetaError> {
        if self.object.is_some() && (!self.replicas.is_empty() || self.ec.is_some()) {
            return Err(MetaError::Invalid(format!(
                "tiered extent {} also lists replicas or shards",
                self.id
            )));
        }
        if let Some(ec) = &self.ec {
            ec.check(self.payload_len, self.replicas.len())?;
            let mut nodes: Vec<&str> = self.replicas.iter().map(|r| r.node_id.as_str()).collect();
            nodes.sort_unstable();
            if nodes.windows(2).any(|w| w[0] == w[1]) {
                return Err(MetaError::Invalid(format!(
                    "extent {} puts two shards on one node",
                    self.id
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeMeta {
    pub id: VolumeId,
    pub name: String,
    pub size_bytes: u64,
    pub extents: BTreeMap<u64, ExtentId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub id: SnapshotId,
    pub volume_id: VolumeId,
    pub name: String,
    /// The volume's size when the snapshot was taken (0 in catalogs written before it was
    /// recorded).
    #[serde(default)]
    pub size_bytes: u64,
    pub extents: BTreeMap<u64, ExtentId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtentMeta {
    pub extent: ExtentRef,
    pub refs: u64,
    pub tombstoned: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub volumes: Tracked<VolumeId, VolumeMeta>,
    pub snapshots: Tracked<SnapshotId, SnapshotMeta>,
    pub extents: Tracked<ExtentId, ExtentMeta>,
    pub applied_index: u64,
    pub current_term: u64,
    #[serde(default)]
    pub free: FreeList,
    /// The Raft voter configuration as of `applied_index` (None until the first change; the
    /// bootstrap voters apply until then).
    #[serde(default)]
    pub membership: Option<Membership>,
    /// Raft transport address (`host:port`) of every node named by a membership change.
    #[serde(default)]
    pub raft_addrs: BTreeMap<String, String>,
    #[serde(default)]
    pub filesystems: Tracked<FsId, FsMeta>,
    #[serde(default)]
    pub fs_snapshots: Tracked<SnapshotId, FsSnapshotMeta>,
    /// Client sessions and the file locks they hold.
    #[serde(default, skip_serializing_if = "leases_empty")]
    pub leases: Leases,
    /// Whether the catalog store holds this catalog apart from the changes the maps track.
    /// False for a catalog built any other way (new, from JSON, from a Raft snapshot), which the
    /// next checkpoint writes in full.
    #[serde(skip)]
    pub in_store: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MetaCommand {
    CreateVolume {
        id: VolumeId,
        name: String,
        size_bytes: u64,
    },
    InstallExtent {
        volume_id: VolumeId,
        logical_offset: u64,
        extent: ExtentRef,
    },
    /// [`MetaCommand::InstallExtent`] for several extents of one write (each at its own
    /// `logical_offset`), committed as one entry.
    InstallExtents {
        volume_id: VolumeId,
        extents: Vec<ExtentRef>,
        /// Ids in `extents` that already exist (content deduplication): they must still be live
        /// when this applies, or the whole command is rejected and the writer places the data
        /// afresh. Anything not listed is installed as new.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        shared: Vec<ExtentId>,
    },
    CreateSnapshot {
        id: SnapshotId,
        volume_id: VolumeId,
        name: String,
    },
    DeleteSnapshot {
        snapshot_id: SnapshotId,
    },
    DeleteVolume {
        volume_id: VolumeId,
    },
    /// Grows a volume; shrinking is refused (it would drop written data).
    ResizeVolume {
        volume_id: VolumeId,
        size_bytes: u64,
    },
    /// A new volume sharing the snapshot's extents (copy-on-write: writes to either side
    /// install new extents). `size_bytes` defaults to the snapshot's size and may only grow it.
    CloneSnapshot {
        id: VolumeId,
        name: String,
        snapshot_id: SnapshotId,
        #[serde(default)]
        size_bytes: Option<u64>,
    },
    MarkExtentReclaimed {
        extent_id: ExtentId,
    },
    /// Moves one replica of an extent to `new` (already written and verified by the repairer):
    /// reserves the new range and returns the old one to the free list.
    ReplaceReplica {
        extent_id: ExtentId,
        old: ReplicaRef,
        new: ReplicaRef,
    },
    /// Moves an extent's data to object `key` (already uploaded and verified by the leader) and
    /// returns its replicas' ranges to the free list. Refused unless the extent still has exactly
    /// `replicas`, so a concurrent repair can't have its new range freed or leaked.
    TierExtent {
        extent_id: ExtentId,
        key: String,
        replicas: Vec<ReplicaRef>,
    },
    /// Raft voter configuration change (joint consensus: a `Joint` entry, then the leader
    /// appends the `Stable` one once it commits). Takes effect in the Raft layer as soon as it
    /// is appended; `addrs` adds or updates transport addresses.
    ChangeMembership {
        membership: Membership,
        #[serde(default)]
        addrs: BTreeMap<String, String>,
    },
    /// A file or directory namespace change ([`crate::namespace`]).
    Fs {
        op: FsOp,
    },
    /// A client session or file lock change ([`crate::leases`]).
    Lease {
        op: LeaseOp,
    },
    /// Appended by a new Raft leader so entries from earlier terms can be committed.
    Noop,
}

fn leases_empty(l: &Leases) -> bool {
    *l == Leases::default()
}

#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error("resource not found: {0}")]
    NotFound(String),
    #[error("invalid metadata transition: {0}")]
    Invalid(String),
    #[error("already exists: {0}")]
    Exists(String),
    #[error("directory not empty: {0}")]
    NotEmpty(String),
    #[error("not a directory: {0}")]
    NotDir(String),
    #[error("is a directory: {0}")]
    IsDir(String),
    #[error("no such extended attribute: {0}")]
    NoAttr(String),
    #[error("too large: {0}")]
    TooBig(String),
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("locked: {0}")]
    Locked(String),
    #[error("quota exceeded: {0}")]
    Quota(String),
    #[error("read-only: {0}")]
    ReadOnly(String),
    /// Session ids are never echoed: one lets its holder renew the session or drop its locks.
    #[error("no such session (expired or closed)")]
    NoSession,
    /// The catalog store could not be read: not a property of the command.
    #[error("catalog store: {0}")]
    Store(String),
}

impl Catalog {
    pub fn apply(
        &mut self,
        term: u64,
        index: u64,
        cmd: &MetaCommand,
    ) -> Result<Vec<ExtentId>, MetaError> {
        let result = self.apply_inner(term, index, cmd);
        if let Err(MetaError::Store(e)) = &result {
            // The command may be half applied, and a replica that could read its store would
            // not reject it: only a restart (checkpoint plus log replay) recovers this state.
            panic!("catalog store unreadable while applying index {index}: {e}");
        }
        result
    }

    fn apply_inner(
        &mut self,
        term: u64,
        index: u64,
        cmd: &MetaCommand,
    ) -> Result<Vec<ExtentId>, MetaError> {
        if index <= self.applied_index {
            return Ok(Vec::new());
        }
        let mut gc_candidates = Vec::new();
        match cmd {
            MetaCommand::CreateVolume {
                id,
                name,
                size_bytes,
            } => {
                // Client-chosen ids make a retried create a no-op instead of a second volume.
                if let Some(v) = self.volumes.get(id) {
                    if v.name == *name && v.size_bytes == *size_bytes {
                        return self.applied(term, index, gc_candidates);
                    }
                    return Err(MetaError::Invalid(format!("volume {id} already exists")));
                }
                self.volumes.insert(
                    id.clone(),
                    VolumeMeta {
                        id: id.clone(),
                        name: name.clone(),
                        size_bytes: *size_bytes,
                        extents: BTreeMap::new(),
                    },
                );
            }
            MetaCommand::InstallExtent {
                volume_id,
                logical_offset,
                extent,
            } => {
                extent.check()?;
                let old = {
                    let vol = self
                        .volumes
                        .get_mut(volume_id)
                        .ok_or_else(|| MetaError::NotFound(volume_id.clone()))?;
                    vol.extents.insert(*logical_offset, extent.id.clone())
                };
                self.add_extent_ref(extent);
                if let Some(old_id) = old {
                    self.dec_ref(&old_id, &mut gc_candidates)?;
                }
            }
            MetaCommand::InstallExtents {
                volume_id,
                extents,
                shared,
            } => {
                for e in extents {
                    e.check()?;
                }
                self.check_shared(shared, extents)?;
                let old: Vec<ExtentId> = {
                    let vol = self
                        .volumes
                        .get_mut(volume_id)
                        .ok_or_else(|| MetaError::NotFound(volume_id.clone()))?;
                    extents
                        .iter()
                        .filter_map(|e| vol.extents.insert(e.logical_offset, e.id.clone()))
                        .collect()
                };
                for e in extents {
                    self.add_extent_ref(e);
                }
                for old_id in old {
                    self.dec_ref(&old_id, &mut gc_candidates)?;
                }
            }
            MetaCommand::CreateSnapshot {
                id,
                volume_id,
                name,
            } => {
                if let Some(s) = self.snapshots.get(id) {
                    if s.volume_id == *volume_id && s.name == *name {
                        return self.applied(term, index, gc_candidates);
                    }
                    return Err(MetaError::Invalid(format!("snapshot {id} already exists")));
                }
                let vol = self
                    .volumes
                    .get(volume_id)
                    .ok_or_else(|| MetaError::NotFound(volume_id.clone()))?
                    .clone();
                for eid in vol.extents.values() {
                    let e = self
                        .extents
                        .get_mut(eid)
                        .ok_or_else(|| MetaError::NotFound(eid.clone()))?;
                    e.refs += 1;
                }
                self.snapshots.insert(
                    id.clone(),
                    SnapshotMeta {
                        id: id.clone(),
                        volume_id: volume_id.clone(),
                        name: name.clone(),
                        size_bytes: vol.size_bytes,
                        extents: vol.extents,
                    },
                );
            }
            MetaCommand::DeleteSnapshot { snapshot_id } => {
                let s = self
                    .snapshots
                    .remove(snapshot_id)
                    .ok_or_else(|| MetaError::NotFound(snapshot_id.clone()))?;
                for eid in s.extents.values() {
                    self.dec_ref(eid, &mut gc_candidates)?;
                }
            }
            MetaCommand::DeleteVolume { volume_id } => {
                let v = self
                    .volumes
                    .remove(volume_id)
                    .ok_or_else(|| MetaError::NotFound(volume_id.clone()))?;
                for eid in v.extents.values() {
                    self.dec_ref(eid, &mut gc_candidates)?;
                }
            }
            MetaCommand::ResizeVolume {
                volume_id,
                size_bytes,
            } => {
                let vol = self
                    .volumes
                    .get_mut(volume_id)
                    .ok_or_else(|| MetaError::NotFound(volume_id.clone()))?;
                if *size_bytes < vol.size_bytes {
                    return Err(MetaError::Invalid(format!(
                        "volume {volume_id} cannot shrink from {} to {size_bytes} bytes",
                        vol.size_bytes
                    )));
                }
                vol.size_bytes = *size_bytes;
            }
            MetaCommand::CloneSnapshot {
                id,
                name,
                snapshot_id,
                size_bytes,
            } => {
                if let Some(v) = self.volumes.get(id) {
                    if v.name == *name {
                        return self.applied(term, index, gc_candidates);
                    }
                    return Err(MetaError::Invalid(format!("volume {id} already exists")));
                }
                let s = self
                    .snapshots
                    .get(snapshot_id)
                    .ok_or_else(|| MetaError::NotFound(snapshot_id.clone()))?
                    .clone();
                let floor = if s.size_bytes > 0 {
                    s.size_bytes
                } else {
                    self.written_end(&s.extents)
                };
                let size = size_bytes.unwrap_or(floor);
                if size == 0 || size < floor {
                    return Err(MetaError::Invalid(format!(
                        "clone of {snapshot_id} needs at least {} bytes",
                        floor.max(1)
                    )));
                }
                for eid in s.extents.values() {
                    self.extents
                        .get_mut(eid)
                        .ok_or_else(|| MetaError::NotFound(eid.clone()))?
                        .refs += 1;
                }
                self.volumes.insert(
                    id.clone(),
                    VolumeMeta {
                        id: id.clone(),
                        name: name.clone(),
                        size_bytes: size,
                        extents: s.extents,
                    },
                );
            }
            MetaCommand::MarkExtentReclaimed { extent_id } => {
                let e = self
                    .extents
                    .get(extent_id)
                    .ok_or_else(|| MetaError::NotFound(extent_id.clone()))?;
                if e.refs != 0 {
                    return Err(MetaError::Invalid(format!(
                        "extent {extent_id} still has {} refs",
                        e.refs
                    )));
                }
                let e = self.extents.remove(extent_id).expect("checked above");
                for r in &e.extent.replicas {
                    self.free
                        .release(&r.node_id, r.device_index, r.offset, e.extent.stored_len())
                        .map_err(MetaError::Invalid)?;
                }
            }
            MetaCommand::TierExtent {
                extent_id,
                key,
                replicas,
            } => {
                let e = self
                    .extents
                    .get_mut(extent_id)
                    .ok_or_else(|| MetaError::NotFound(extent_id.clone()))?;
                if e.extent.object.is_some() {
                    return Err(MetaError::Invalid(format!(
                        "extent {extent_id} is already tiered"
                    )));
                }
                if &e.extent.replicas != replicas || replicas.is_empty() {
                    return Err(MetaError::Invalid(format!(
                        "extent {extent_id} changed before it could be tiered"
                    )));
                }
                let len = e.extent.stored_len();
                let old = std::mem::take(&mut e.extent.replicas);
                e.extent.ec = None;
                e.extent.object = Some(key.clone());
                for r in &old {
                    self.free
                        .release(&r.node_id, r.device_index, r.offset, len)
                        .map_err(MetaError::Invalid)?;
                }
            }
            MetaCommand::ReplaceReplica {
                extent_id,
                old,
                new,
            } => {
                if old == new {
                    return Err(MetaError::Invalid(
                        "replacement replica is the same range".into(),
                    ));
                }
                let e = self
                    .extents
                    .get_mut(extent_id)
                    .ok_or_else(|| MetaError::NotFound(extent_id.clone()))?;
                let pos = e
                    .extent
                    .replicas
                    .iter()
                    .position(|r| r == old)
                    .ok_or_else(|| {
                        MetaError::Invalid(format!(
                            "extent {extent_id} has no replica on {} at {}",
                            old.node_id, old.offset
                        ))
                    })?;
                if e.extent
                    .replicas
                    .iter()
                    .enumerate()
                    .any(|(i, r)| i != pos && r.node_id == new.node_id)
                {
                    return Err(MetaError::Invalid(format!(
                        "extent {extent_id} already has a replica on {}",
                        new.node_id
                    )));
                }
                let len = e.extent.stored_len();
                e.extent.replicas[pos] = new.clone();
                self.free
                    .reserve(&new.node_id, new.device_index, new.offset, len);
                self.free
                    .release(&old.node_id, old.device_index, old.offset, len)
                    .map_err(MetaError::Invalid)?;
            }
            MetaCommand::ChangeMembership { membership, addrs } => {
                if membership.voters().is_empty() {
                    return Err(MetaError::Invalid("membership has no voters".into()));
                }
                if let Membership::Joint { old, new } = membership {
                    if old.is_empty() || new.is_empty() {
                        return Err(MetaError::Invalid(
                            "joint membership needs voters on both sides".into(),
                        ));
                    }
                }
                self.raft_addrs
                    .extend(addrs.iter().map(|(k, v)| (k.clone(), v.clone())));
                self.membership = Some(membership.clone());
            }
            MetaCommand::Fs { op } => self.apply_fs(op, &mut gc_candidates)?,
            MetaCommand::Lease { op } => {
                let filesystems = &self.filesystems;
                self.leases.apply(op, term, |fs, ino| {
                    let f = filesystems
                        .get(fs)
                        .ok_or_else(|| MetaError::NotFound(format!("filesystem {fs}")))?;
                    match f.inode(ino)?.kind {
                        crate::namespace::InodeKind::File { .. } => Ok(()),
                        _ => Err(MetaError::Invalid(format!(
                            "inode {ino} is not a regular file"
                        ))),
                    }
                })?;
            }
            MetaCommand::Noop => {}
        }
        self.applied(term, index, gc_candidates)
    }

    fn applied(
        &mut self,
        term: u64,
        index: u64,
        gc: Vec<ExtentId>,
    ) -> Result<Vec<ExtentId>, MetaError> {
        self.current_term = term;
        self.applied_index = index;
        Ok(gc)
    }

    /// Applies an already-committed entry. A command that fails validation still consumes its
    /// index and leaves the catalog otherwise untouched, so every replica that applies the same
    /// log reaches the same state.
    ///
    /// `apply` checks every input before it changes anything; only a catalog that is already
    /// inconsistent (a referenced extent missing, a free range released twice) can fail halfway,
    /// and then every replica fails the same way. Debug builds verify the no-change part.
    pub fn apply_committed(
        &mut self,
        term: u64,
        index: u64,
        cmd: &MetaCommand,
    ) -> Result<Vec<ExtentId>, MetaError> {
        #[cfg(debug_assertions)]
        let before = serde_json::to_value(&*self).expect("catalog serializes");
        let result = self.apply(term, index, cmd);
        if result.is_err() {
            #[cfg(debug_assertions)]
            assert_eq!(
                serde_json::to_value(&*self).expect("catalog serializes"),
                before,
                "a rejected {cmd:?} changed the catalog"
            );
            if index > self.applied_index {
                self.applied_index = index;
                self.current_term = term;
            }
        }
        result
    }

    /// End of the last written byte among `extents`.
    pub fn written_end(&self, extents: &BTreeMap<u64, ExtentId>) -> u64 {
        extents
            .iter()
            .filter_map(|(at, eid)| self.extents.get(eid).map(|m| at + m.extent.len() as u64))
            .max()
            .unwrap_or(0)
    }

    /// Rejects a command that claims to share extents (`shared`) that are gone, being freed, or
    /// no longer the same bytes. Nothing has changed yet when this runs.
    pub(crate) fn check_shared(
        &self,
        shared: &[ExtentId],
        extents: &[ExtentRef],
    ) -> Result<(), MetaError> {
        for id in shared {
            let live = self
                .extents
                .get(id)
                .filter(|m| !m.tombstoned && m.refs > 0)
                .ok_or_else(|| MetaError::Invalid(format!("shared extent {id} is gone")))?;
            let same = extents.iter().any(|e| {
                e.id == *id
                    && e.checksum == live.extent.checksum
                    && e.payload_len == live.extent.payload_len
                    && e.raw_len == live.extent.raw_len
            });
            if !same {
                return Err(MetaError::Invalid(format!(
                    "shared extent {id} does not match the catalog"
                )));
            }
        }
        Ok(())
    }

    /// Takes a reference on `extent`, reserving its device ranges the first time it is seen.
    pub(crate) fn add_extent_ref(&mut self, extent: &ExtentRef) {
        if !self.extents.contains_key(&extent.id) {
            for r in &extent.replicas {
                self.free
                    .reserve(&r.node_id, r.device_index, r.offset, extent.stored_len());
            }
        }
        if let Some(e) = self.extents.get_mut(&extent.id) {
            e.refs += 1;
        } else {
            self.extents.insert(
                extent.id.clone(),
                ExtentMeta {
                    extent: extent.clone(),
                    refs: 1,
                    tombstoned: false,
                },
            );
        }
    }

    pub(crate) fn dec_ref(
        &mut self,
        extent_id: &str,
        gc: &mut Vec<ExtentId>,
    ) -> Result<(), MetaError> {
        let e = self
            .extents
            .get_mut(extent_id)
            .ok_or_else(|| MetaError::NotFound(extent_id.to_string()))?;
        if e.refs == 0 {
            return Err(MetaError::Invalid(format!(
                "extent {extent_id} refcount underflow"
            )));
        }
        e.refs -= 1;
        if e.refs == 0 {
            gc.push(extent_id.to_string());
        }
        Ok(())
    }
}
