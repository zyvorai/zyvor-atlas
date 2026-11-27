// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Replica filesystems: the receiving side of asynchronous replication. A source's snapshots
//! arrive as increments ([`FsOp::ReplicaApply`] parts, then [`FsOp::ReplicaCommit`]), keeping
//! the source's inode numbers, so the replica's tree after each commit equals the source
//! snapshot's. Promotion and demotion revert to a complete increment, never a partial one.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    check_name, check_xattr_name, node_type, FsMeta, Inode, InodeKind, NodeType, ReplicaInode,
    ReplicaState, MAX_SYMLINK_BYTES, MAX_XATTR_TOTAL_BYTES, MAX_XATTR_VALUE_BYTES, ROOT_INO,
};
use crate::{
    inodes::DirEntries,
    metadata::{Catalog, DeltaRef, DeltaTarget, ExtentId, MetaError, SnapshotId},
};

fn invalid(msg: String) -> MetaError {
    MetaError::Invalid(msg)
}

fn empty_replica(fs: &str, name: &str, extent_bytes: u64, now_ns: i64) -> FsMeta {
    let root = Inode::new(
        ROOT_INO,
        InodeKind::Dir {
            parent: ROOT_INO,
            entries: DirEntries::default(),
        },
        0o755,
        0,
        0,
        now_ns,
    );
    FsMeta {
        id: fs.to_string(),
        name: name.to_string(),
        next_ino: ROOT_INO + 1,
        inodes: [root].into_iter().collect(),
        source_snapshot: None,
        extent_bytes: Some(extent_bytes),
        usage: Some(super::FsUsage::default()),
        quota: None,
        replica: Some(ReplicaState::default()),
    }
}

impl Catalog {
    fn replica_of(&self, fs: &str) -> Result<&ReplicaState, MetaError> {
        self.filesystem(fs)?
            .replica
            .as_ref()
            .ok_or_else(|| invalid(format!("filesystem {fs} is not a replica")))
    }

    pub(super) fn create_replica(
        &mut self,
        fs: &str,
        name: &str,
        now_ns: i64,
        extent_bytes: u64,
    ) -> Result<(), MetaError> {
        if extent_bytes == 0 {
            return Err(invalid("extent_bytes must be positive".into()));
        }
        if let Some(f) = self.filesystems.get(fs) {
            if f.name == name && f.replica.is_some() && f.extent_bytes == Some(extent_bytes) {
                return Ok(());
            }
            return Err(MetaError::Exists(format!("filesystem {fs}")));
        }
        self.filesystems.insert(
            fs.to_string(),
            empty_replica(fs, name, extent_bytes, now_ns),
        );
        Ok(())
    }

    /// Drops a partly applied increment: back to the last complete one, or to an empty root.
    fn discard_partial(&mut self, fs: &str, gc: &mut Vec<ExtentId>) -> Result<(), MetaError> {
        let r = self.replica_of(fs)?.clone();
        if let Some(local) = &r.local {
            let state = ReplicaState {
                pending: None,
                ..r.clone()
            };
            return self.revert_to(fs, local, Some(state), gc);
        }
        let f = self.filesystem(fs)?;
        let old = f.inodes.scan()?;
        let root_ns = f.inode(ROOT_INO)?.ctime_ns;
        let mut fresh = empty_replica(fs, &f.name, f.extent_bytes.unwrap_or_default(), root_ns);
        fresh.quota = f.quota;
        fresh.next_ino = f.next_ino;
        self.filesystems.insert(fs.to_string(), fresh);
        self.leases.forget_fs(fs);
        for inode in &old {
            self.drop_inode(fs, inode, gc)?;
        }
        Ok(())
    }

    /// Checks one part of an increment against the replica before anything changes.
    fn check_part(
        &self,
        fs: &str,
        from: Option<&SnapshotId>,
        to: &SnapshotId,
        inodes: &[ReplicaInode],
        removed: &[u64],
    ) -> Result<(), MetaError> {
        let r = self.replica_of(fs)?;
        if r.pending.as_ref() != Some(to) && r.base.as_ref() != from {
            return Err(invalid(format!(
                "increment from {from:?} does not start at replica {fs}'s base {:?}",
                r.base
            )));
        }
        let f = self.filesystem(fs)?;
        let mut types: BTreeMap<u64, NodeType> = BTreeMap::new();
        for n in inodes {
            if n.ino == 0 {
                return Err(invalid("inode 0 does not exist".into()));
            }
            if n.ino == ROOT_INO && n.node_type != NodeType::Dir {
                return Err(invalid("the root inode is a directory".into()));
            }
            let known = match types.get(&n.ino) {
                Some(t) => Some(*t),
                None => f.inodes.get(n.ino)?.map(|i| node_type(&i)),
            };
            if known.is_some_and(|t| t != n.node_type) {
                return Err(invalid(format!(
                    "inode {} would change type to {:?}",
                    n.ino, n.node_type
                )));
            }
            types.insert(n.ino, n.node_type);
            match n.node_type {
                NodeType::Symlink => {
                    if !n
                        .target
                        .as_ref()
                        .is_some_and(|t| !t.is_empty() && t.len() <= MAX_SYMLINK_BYTES)
                    {
                        return Err(invalid(format!(
                            "symlink {} needs a 1-{MAX_SYMLINK_BYTES} byte target",
                            n.ino
                        )));
                    }
                }
                NodeType::Dir if n.parent == 0 => {
                    return Err(invalid(format!("directory {} needs a parent", n.ino)));
                }
                _ => {}
            }
            if n.node_type != NodeType::Dir && !n.entries.is_empty() {
                return Err(invalid(format!(
                    "inode {} has entries but is no directory",
                    n.ino
                )));
            }
            if n.node_type != NodeType::File && !n.holes.is_empty() {
                return Err(invalid(format!("inode {} has holes but is no file", n.ino)));
            }
            for (name, _) in &n.entries {
                check_name(name)?;
            }
            let mut total = 0;
            for (name, value) in &n.xattrs {
                check_xattr_name(name)?;
                if value.len() > MAX_XATTR_VALUE_BYTES {
                    return Err(MetaError::TooBig(format!(
                        "extended attribute values are at most {MAX_XATTR_VALUE_BYTES} bytes"
                    )));
                }
                total += name.len() + value.len();
            }
            if total > MAX_XATTR_TOTAL_BYTES {
                return Err(MetaError::TooBig(format!(
                    "extended attributes of inode {} exceed {MAX_XATTR_TOTAL_BYTES} bytes",
                    n.ino
                )));
            }
        }
        let upserted: BTreeSet<u64> = types.keys().copied().collect();
        for ino in removed {
            if *ino == ROOT_INO {
                return Err(invalid("the root inode cannot be removed".into()));
            }
            if upserted.contains(ino) {
                return Err(invalid(format!("inode {ino} is both changed and removed")));
            }
        }
        Ok(())
    }

    pub(super) fn replica_apply(
        &mut self,
        fs: &str,
        from: Option<&SnapshotId>,
        to: &SnapshotId,
        inodes: &[ReplicaInode],
        removed: &[u64],
        gc: &mut Vec<ExtentId>,
    ) -> Result<(), MetaError> {
        self.check_part(fs, from, to, inodes, removed)?;
        if self
            .replica_of(fs)?
            .pending
            .as_ref()
            .is_some_and(|p| p != to)
        {
            self.discard_partial(fs, gc)?;
        }
        let mut dropped: Vec<ExtentId> = Vec::new();
        // Delta lists of cells this increment cuts or clears: (inode, cell, list).
        let mut stale: Vec<(u64, u64, Vec<DeltaRef>)> = Vec::new();
        let mut gone: Vec<Inode> = Vec::new();
        let mut file_bytes: i128 = 0;
        {
            let f = self.fs_mut(fs)?;
            for n in inodes {
                match f.inodes.get_mut(n.ino)? {
                    Some(i) => {
                        i.mode = n.mode & 0o7777;
                        i.uid = n.uid;
                        i.gid = n.gid;
                        i.nlink = n.nlink;
                        i.atime_ns = n.atime_ns;
                        i.mtime_ns = n.mtime_ns;
                        i.ctime_ns = n.ctime_ns;
                        i.xattrs = n.xattrs.clone();
                        match &mut i.kind {
                            InodeKind::File {
                                size,
                                extents,
                                deltas,
                            } => {
                                if n.size < *size {
                                    dropped.extend(extents.split_off(&n.size).into_values());
                                    let cut = deltas.split_off(&n.size);
                                    stale.extend(cut.into_iter().map(|(c, l)| (n.ino, c, l)));
                                }
                                for h in &n.holes {
                                    dropped.extend(extents.remove(h));
                                    stale.extend(deltas.remove(h).map(|l| (n.ino, *h, l)));
                                }
                                file_bytes += i128::from(n.size) - i128::from(*size);
                                *size = n.size;
                            }
                            InodeKind::Dir { parent, .. } => *parent = n.parent,
                            InodeKind::Symlink { target } => {
                                target.clone_from(n.target.as_ref().expect("checked"))
                            }
                            InodeKind::Special { rdev, .. } => *rdev = n.rdev,
                        }
                    }
                    None => {
                        let kind = match n.node_type {
                            NodeType::File => InodeKind::File {
                                size: n.size,
                                extents: BTreeMap::new(),
                                deltas: BTreeMap::new(),
                            },
                            NodeType::Dir => InodeKind::Dir {
                                parent: n.parent,
                                entries: DirEntries::default(),
                            },
                            NodeType::Symlink => InodeKind::Symlink {
                                target: n.target.clone().expect("checked"),
                            },
                            t => InodeKind::Special {
                                node_type: t,
                                rdev: n.rdev,
                            },
                        };
                        if n.node_type == NodeType::File {
                            file_bytes += i128::from(n.size);
                        }
                        f.inodes.insert(Inode {
                            ino: n.ino,
                            kind,
                            mode: n.mode & 0o7777,
                            uid: n.uid,
                            gid: n.gid,
                            nlink: n.nlink,
                            atime_ns: n.atime_ns,
                            mtime_ns: n.mtime_ns,
                            ctime_ns: n.ctime_ns,
                            op_id: String::new(),
                            xattrs: n.xattrs.clone(),
                        });
                    }
                }
                f.next_ino = f.next_ino.max(n.ino + 1);
                for (name, ino) in &n.entries {
                    f.inodes.set_entry(n.ino, name, *ino)?;
                }
            }
            for ino in removed {
                gone.extend(f.inodes.remove(*ino)?);
            }
            f.replica.as_mut().expect("checked").pending = Some(to.clone());
        }
        let dropped_bytes: u64 = dropped.iter().map(|e| self.extent_len(e)).sum();
        if let Some(u) = self.usage_mut(fs) {
            u.file_bytes = (i128::from(u.file_bytes) + file_bytes).max(0) as u64;
            u.used_bytes = u.used_bytes.saturating_sub(dropped_bytes);
        }
        for e in &dropped {
            self.dec_ref(e, gc)?;
        }
        for (ino, cell, list) in stale {
            let target = DeltaTarget::File {
                fs: fs.to_string(),
                ino,
            };
            self.drop_deltas(target, cell, list, gc)?;
        }
        for inode in &gone {
            self.forget_usage(fs, inode);
            self.drop_inode(fs, inode, gc)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn replica_commit(
        &mut self,
        fs: &str,
        from: Option<&SnapshotId>,
        to: &SnapshotId,
        snapshot: &SnapshotId,
        name: &str,
        next_ino: u64,
        now_ns: i64,
        gc: &mut Vec<ExtentId>,
    ) -> Result<(), MetaError> {
        let r = self.replica_of(fs)?;
        if r.base.as_ref() == Some(to) && r.local.as_ref() == Some(snapshot) {
            return Ok(());
        }
        let started = r.pending.as_ref() == Some(to);
        if !started && r.base.as_ref() != from {
            return Err(invalid(format!(
                "increment from {from:?} to {to} does not continue replica {fs} (base {:?}, \
                 pending {:?})",
                r.base, r.pending
            )));
        }
        let partial = !started && r.pending.is_some();
        if self.fs_snapshots.contains_key(snapshot) {
            return Err(MetaError::Exists(format!("filesystem snapshot {snapshot}")));
        }
        if partial {
            self.discard_partial(fs, gc)?;
        }
        let f = self.fs_mut(fs)?;
        f.next_ino = f.next_ino.max(next_ino);
        f.replica = Some(ReplicaState {
            base: Some(to.clone()),
            local: Some(snapshot.clone()),
            pending: None,
        });
        self.snapshot_tree(snapshot, fs, name, now_ns)
    }

    pub(super) fn replica_promote(
        &mut self,
        fs: &str,
        force: bool,
        gc: &mut Vec<ExtentId>,
    ) -> Result<(), MetaError> {
        let Some(r) = self.filesystem(fs)?.replica.clone() else {
            return Ok(());
        };
        match (r.local, r.pending) {
            (Some(local), Some(_)) => self.revert_to(fs, &local, None, gc),
            (Some(_), None) => {
                self.fs_mut(fs)?.replica = None;
                Ok(())
            }
            (None, _) if force => {
                self.fs_mut(fs)?.replica = None;
                Ok(())
            }
            (None, _) => Err(invalid(format!(
                "replica {fs} has no complete increment yet; force promotes it as it is"
            ))),
        }
    }

    pub(super) fn replica_demote(
        &mut self,
        fs: &str,
        snapshot: &SnapshotId,
        base: &SnapshotId,
        gc: &mut Vec<ExtentId>,
    ) -> Result<(), MetaError> {
        if let Some(r) = &self.filesystem(fs)?.replica {
            if r.local.as_ref() == Some(snapshot) && r.base.as_ref() == Some(base) {
                return Ok(());
            }
            return Err(invalid(format!("filesystem {fs} is already a replica")));
        }
        let state = ReplicaState {
            base: Some(base.clone()),
            local: Some(snapshot.clone()),
            pending: None,
        };
        self.revert_to(fs, snapshot, Some(state), gc)
    }

    /// Replaces `fs`'s tree with that of its snapshot `snapshot`, keeping its identity and quota.
    fn revert_to(
        &mut self,
        fs: &str,
        snapshot: &SnapshotId,
        replica: Option<ReplicaState>,
        gc: &mut Vec<ExtentId>,
    ) -> Result<(), MetaError> {
        let s = self
            .fs_snapshots
            .get(snapshot)
            .filter(|s| s.fs_id == fs)
            .ok_or_else(|| {
                MetaError::NotFound(format!("snapshot {snapshot} of filesystem {fs}"))
            })?;
        let mut tree = s.tree.clone();
        let current = self.filesystem(fs)?;
        let old = current.inodes.scan()?;
        tree.id = current.id.clone();
        tree.name = current.name.clone();
        tree.quota = current.quota;
        tree.source_snapshot = current.source_snapshot.clone();
        tree.next_ino = tree.next_ino.max(current.next_ino);
        tree.replica = replica;
        self.share_extents(&tree.inodes)?;
        self.filesystems.insert(fs.to_string(), tree);
        self.leases.forget_fs(fs);
        for inode in &old {
            self.drop_inode(fs, inode, gc)?;
        }
        Ok(())
    }
}
