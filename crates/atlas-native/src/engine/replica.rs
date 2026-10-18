// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Asynchronous filesystem replication: what changed between two snapshots of a filesystem
//! ([`NativeEngine::fs_diff`]), and the replica side that applies it (see
//! `namespace::replica`). File data moves per extent: only grid cells whose extent differs
//! between the snapshots are copied.

use std::{collections::BTreeMap, sync::Arc};

use serde::{Deserialize, Serialize};

use super::{now_ns, NativeEngine, NativeError};
use crate::{
    metadata::{Catalog, MetaCommand, MetaError, SnapshotId},
    namespace::{node_type, FsId, FsMeta, FsOp, Inode, InodeKind, ReplicaInode},
};

/// Inodes one diff page returns unless the request says otherwise.
pub const DEFAULT_DIFF_LIMIT: usize = 512;

/// A changed inode and the byte ranges of its file data to copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffInode {
    #[serde(flatten)]
    pub inode: ReplicaInode,
    /// File: `(offset, len)` ranges whose extents changed, each within one grid cell.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data: Vec<(u64, u64)>,
}

/// One page of the changes from one snapshot of a filesystem to a later one, in inode order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffPage {
    pub fs: FsId,
    pub extent_bytes: u64,
    /// The newer snapshot's next inode number.
    pub next_ino: u64,
    pub inodes: Vec<DiffInode>,
    /// Inodes the older snapshot has and the newer doesn't.
    pub removed: Vec<u64>,
    /// Pass as `after` for the next page; `None` on the last.
    pub next: Option<u64>,
}

fn tree<'a>(c: &'a Catalog, id: &str) -> Result<(&'a FsId, &'a FsMeta), NativeError> {
    c.fs_snapshots
        .get(id)
        .map(|s| (&s.fs_id, &s.tree))
        .ok_or_else(|| MetaError::NotFound(format!("filesystem snapshot {id}")).into())
}

/// Whether two versions of an inode differ in anything a replica keeps. A directory's entries
/// count as unchanged while its ctime is: every entry change stamps it.
fn same(a: &Inode, b: &Inode) -> bool {
    if node_type(a) != node_type(b)
        || (a.mode, a.uid, a.gid, a.nlink) != (b.mode, b.uid, b.gid, b.nlink)
        || (a.atime_ns, a.mtime_ns, a.ctime_ns) != (b.atime_ns, b.mtime_ns, b.ctime_ns)
        || a.xattrs != b.xattrs
    {
        return false;
    }
    match (&a.kind, &b.kind) {
        (
            InodeKind::File {
                size: sa,
                extents: ea,
            },
            InodeKind::File {
                size: sb,
                extents: eb,
            },
        ) => sa == sb && ea == eb,
        (InodeKind::Dir { parent: pa, .. }, InodeKind::Dir { parent: pb, .. }) => pa == pb,
        (InodeKind::Symlink { target: ta }, InodeKind::Symlink { target: tb }) => ta == tb,
        (InodeKind::Special { rdev: ra, .. }, InodeKind::Special { rdev: rb, .. }) => ra == rb,
        _ => false,
    }
}

/// `new`'s entries as set/remove changes from `old`'s (both with every entry in memory).
fn entry_changes(old: Option<&Inode>, new: &Inode) -> Vec<(String, Option<u64>)> {
    let entries = |i: &Inode| -> BTreeMap<String, u64> {
        match &i.kind {
            InodeKind::Dir { entries, .. } => {
                entries.in_memory().map(|(k, v)| (k.clone(), *v)).collect()
            }
            _ => BTreeMap::new(),
        }
    };
    let before = old.map(entries).unwrap_or_default();
    let after = entries(new);
    let mut out: Vec<(String, Option<u64>)> = after
        .iter()
        .filter(|(k, v)| before.get(*k) != Some(*v))
        .map(|(k, v)| (k.clone(), Some(*v)))
        .collect();
    out.extend(
        before
            .keys()
            .filter(|k| !after.contains_key(*k))
            .map(|k| (k.clone(), None)),
    );
    out.sort();
    out
}

fn diff_inode(c: &Catalog, old: Option<&Inode>, new: &Inode) -> Option<DiffInode> {
    let old = old.filter(|o| node_type(o) == node_type(new));
    if old.is_some_and(|o| same(o, new)) {
        return None;
    }
    let mut r = ReplicaInode {
        ino: new.ino,
        node_type: node_type(new),
        mode: new.mode,
        uid: new.uid,
        gid: new.gid,
        nlink: new.nlink,
        atime_ns: new.atime_ns,
        mtime_ns: new.mtime_ns,
        ctime_ns: new.ctime_ns,
        xattrs: new.xattrs.clone(),
        size: 0,
        holes: Vec::new(),
        parent: 0,
        entries: Vec::new(),
        target: None,
        rdev: 0,
    };
    let mut data = Vec::new();
    match &new.kind {
        InodeKind::File { size, extents } => {
            r.size = *size;
            let before = match old.map(|o| &o.kind) {
                Some(InodeKind::File { extents, .. }) => Some(extents),
                _ => None,
            };
            for (off, id) in extents {
                if before.and_then(|b| b.get(off)) == Some(id) || *off >= *size {
                    continue;
                }
                let len = c.extents.get(id).map_or(0, |m| m.extent.len() as u64);
                let len = len.min(size - off);
                if len > 0 {
                    data.push((*off, len));
                }
            }
            // Cleared before the data arrives: a write into a cell still holding the old extent
            // would merge the old bytes past the new ones back in.
            if let Some(b) = before {
                r.holes = b
                    .iter()
                    .filter(|(off, id)| **off < *size && extents.get(off) != Some(id))
                    .map(|(off, _)| *off)
                    .collect();
            }
        }
        InodeKind::Dir { parent, .. } => {
            r.parent = *parent;
            r.entries = entry_changes(old, new);
        }
        InodeKind::Symlink { target } => r.target = Some(target.clone()),
        InodeKind::Special { rdev, .. } => r.rdev = *rdev,
    }
    Some(DiffInode { inode: r, data })
}

pub(crate) fn diff_page(
    c: &Catalog,
    cluster_grid: u64,
    to: &str,
    from: Option<&str>,
    after: u64,
    limit: usize,
) -> Result<DiffPage, NativeError> {
    let limit = limit.max(1);
    let (fs, new) = tree(c, to)?;
    let old = match from {
        Some(id) => {
            let (ofs, old) = tree(c, id)?;
            if ofs != fs {
                return Err(NativeError::Invalid(format!(
                    "snapshots {id} and {to} belong to different filesystems"
                )));
            }
            Some(old)
        }
        None => None,
    };
    let start = after.saturating_add(1);
    let page: Vec<Arc<Inode>> = new.inodes.page_full(start, limit)?;
    let end = match page.last() {
        Some(last) if page.len() == limit => last.ino,
        _ => u64::MAX,
    };
    let mut removed = Vec::new();
    if let Some(old) = old {
        let mut cursor = start;
        'scan: loop {
            let batch = old.inodes.page(cursor, 1024)?;
            for i in &batch {
                if i.ino > end {
                    break 'scan;
                }
                if new.inodes.get(i.ino)?.is_none() {
                    removed.push(i.ino);
                }
            }
            match batch.last() {
                Some(last) if batch.len() == 1024 => cursor = last.ino + 1,
                _ => break,
            }
        }
    }
    let mut inodes = Vec::new();
    for i in &page {
        let before = match old {
            Some(o) => match o.inodes.get(i.ino)? {
                Some(b) => Some(o.inodes.full(b)?),
                None => None,
            },
            None => None,
        };
        inodes.extend(diff_inode(c, before.as_deref(), i));
    }
    Ok(DiffPage {
        fs: fs.clone(),
        extent_bytes: new.extent_bytes.unwrap_or(cluster_grid),
        next_ino: new.next_ino,
        inodes,
        removed,
        next: (end != u64::MAX).then_some(end),
    })
}

impl NativeEngine {
    /// The changes from snapshot `from` (`None`: an empty filesystem) to snapshot `to` of the
    /// same filesystem, for inodes numbered above `after`, at most `limit` of the newer
    /// snapshot's inodes per page.
    pub fn fs_diff(
        &self,
        to: &str,
        from: Option<&str>,
        after: u64,
        limit: usize,
    ) -> Result<DiffPage, NativeError> {
        let grid = self.cfg.extent_bytes as u64;
        self.with_catalog(|c| diff_page(c, grid, to, from, after, limit))?
    }

    fn replica_commit_op(&self, op: FsOp) -> Result<(), NativeError> {
        self.commit(MetaCommand::Fs { op }, None)
    }

    /// An empty replica filesystem on grid `extent_bytes` (the source's), idempotent.
    pub fn create_replica(
        &self,
        id: String,
        name: impl Into<String>,
        extent_bytes: u64,
    ) -> Result<FsId, NativeError> {
        let cluster = self.cfg.extent_bytes as u64;
        if extent_bytes == 0 || extent_bytes > cluster {
            return Err(NativeError::Invalid(format!(
                "extent_bytes must be 1..={cluster}, got {extent_bytes}"
            )));
        }
        self.replica_commit_op(FsOp::CreateReplica {
            fs: id.clone(),
            name: name.into(),
            now_ns: now_ns(),
            extent_bytes,
        })?;
        Ok(id)
    }

    /// Applies one part of the increment from source snapshot `from` to `to`.
    pub fn replica_apply(
        &self,
        fs: &str,
        from: Option<SnapshotId>,
        to: SnapshotId,
        inodes: Vec<ReplicaInode>,
        removed: Vec<u64>,
    ) -> Result<(), NativeError> {
        self.replica_commit_op(FsOp::ReplicaApply {
            fs: fs.into(),
            from,
            to,
            inodes,
            removed,
        })
    }

    /// Completes increment `to`, keeping the replica's state as its snapshot `snapshot`.
    pub fn replica_commit(
        &self,
        fs: &str,
        from: Option<SnapshotId>,
        to: SnapshotId,
        snapshot: SnapshotId,
        next_ino: u64,
    ) -> Result<(), NativeError> {
        self.replica_commit_op(FsOp::ReplicaCommit {
            fs: fs.into(),
            from,
            name: to.clone(),
            to,
            snapshot,
            next_ino,
            now_ns: now_ns(),
        })
    }

    /// Makes a replica writable at its last complete increment.
    pub fn replica_promote(&self, fs: &str, force: bool) -> Result<(), NativeError> {
        self.replica_commit_op(FsOp::ReplicaPromote {
            fs: fs.into(),
            force,
        })
    }

    /// Reverts writable `fs` to its snapshot `snapshot` and makes it a replica at source
    /// snapshot `base`.
    pub fn replica_demote(
        &self,
        fs: &str,
        snapshot: SnapshotId,
        base: SnapshotId,
    ) -> Result<(), NativeError> {
        self.replica_commit_op(FsOp::ReplicaDemote {
            fs: fs.into(),
            snapshot,
            base,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        metadata::{ExtentRef, ReplicaRef},
        namespace::{NodeType, SetAttr, XattrMode, ROOT_INO},
    };

    const GRID: u64 = 4096;

    #[derive(Default)]
    struct H {
        c: Catalog,
        index: u64,
        now: i64,
    }

    impl H {
        fn run(&mut self, op: FsOp) -> Result<(), MetaError> {
            self.index += 1;
            let r = self
                .c
                .apply_committed(1, self.index, &MetaCommand::Fs { op })
                .map(|_| ());
            // Every op keeps the usage counters equal to a fresh count.
            let mut counted = self.c.clone();
            for f in counted.filesystems.values_mut() {
                f.usage = None;
            }
            let ids: Vec<String> = counted.fs_snapshots.keys().cloned().collect();
            for id in ids {
                counted.fs_snapshots.get_mut(&id).unwrap().tree.usage = None;
            }
            counted.fill_usage().unwrap();
            assert_eq!(
                serde_json::to_value(&counted).unwrap(),
                serde_json::to_value(&self.c).unwrap(),
                "usage drifted from a fresh count"
            );
            r
        }

        fn ok(&mut self, op: FsOp) {
            self.run(op).expect("op applies");
        }

        fn now(&mut self) -> i64 {
            self.now += 1;
            self.now
        }

        fn mk(&mut self, fs: &str, parent: u64, name: &str, t: NodeType) -> u64 {
            let now_ns = self.now();
            self.ok(FsOp::Mknode {
                fs: fs.into(),
                parent,
                name: name.into(),
                op_id: format!("{fs}-{parent}-{name}-{now_ns}"),
                node_type: t,
                target: (t == NodeType::Symlink).then(|| "target".to_string()),
                rdev: 0,
                mode: 0o644,
                create_mode: None,
                uid: 1000,
                gid: 1000,
                now_ns,
            });
            self.c.filesystem(fs).unwrap().lookup(parent, name).unwrap()
        }

        fn write(&mut self, fs: &str, ino: u64, off: u64, id: &str, len: usize) {
            let now_ns = self.now();
            self.ok(FsOp::InstallFileExtent {
                fs: fs.into(),
                ino,
                logical_offset: off,
                extent: ExtentRef {
                    id: id.into(),
                    logical_offset: off,
                    payload_len: len,
                    raw_len: 0,
                    checksum: [0; 32],
                content: None,
                    replicas: vec![ReplicaRef {
                        node_id: "n".into(),
                        device_index: 0,
                        offset: self.index * GRID,
                    }],
                    ec: None,
                    created_ms: 0,
                    object: None,
                },
                size: off + len as u64,
                now_ns,
                replica: false,
            });
        }

        fn snapshot(&mut self, fs: &str, id: &str) {
            let now_ns = self.now();
            self.ok(FsOp::SnapshotFs {
                id: id.into(),
                fs: fs.into(),
                name: id.into(),
                now_ns,
            });
        }

        /// What the replicator does, with data "copied" by sharing the source's extents.
        fn replicate(&mut self, dst: &str, from: Option<&str>, to: &str, local: &str) {
            let mut after = 0;
            loop {
                let page = diff_page(&self.c, GRID, to, from, after, 2).unwrap();
                self.ok(FsOp::ReplicaApply {
                    fs: dst.into(),
                    from: from.map(str::to_string),
                    to: to.into(),
                    inodes: page.inodes.iter().map(|d| d.inode.clone()).collect(),
                    removed: page.removed.clone(),
                });
                for d in &page.inodes {
                    for (off, len) in &d.data {
                        let src = self.c.fs_snapshots[to].tree.inode(d.inode.ino).unwrap();
                        let InodeKind::File { extents, .. } = &src.kind else {
                            panic!("data for a non-file");
                        };
                        let mut ext = self.c.extents[&extents[off]].extent.clone();
                        ext.logical_offset = *off;
                        self.ok(FsOp::InstallFileExtent {
                            fs: dst.into(),
                            ino: d.inode.ino,
                            logical_offset: *off,
                            extent: ext,
                            size: off + len,
                            now_ns: 0,
                            replica: true,
                        });
                    }
                }
                match page.next {
                    Some(n) => after = n,
                    None => {
                        let now_ns = self.now();
                        self.ok(FsOp::ReplicaCommit {
                            fs: dst.into(),
                            from: from.map(str::to_string),
                            to: to.into(),
                            snapshot: local.into(),
                            name: to.into(),
                            next_ino: page.next_ino,
                            now_ns,
                        });
                        return;
                    }
                }
            }
        }

        fn assert_same(&self, a: &FsMeta, b: &FsMeta) {
            let ia = a.inodes.scan_full().unwrap();
            let ib = b.inodes.scan_full().unwrap();
            assert_eq!(
                ia.iter().map(|i| i.ino).collect::<Vec<_>>(),
                ib.iter().map(|i| i.ino).collect::<Vec<_>>()
            );
            for (x, y) in ia.iter().zip(&ib) {
                assert!(same(x, y), "inode {} differs:\n{x:?}\n{y:?}", x.ino);
                assert!(entry_changes(Some(x), y).is_empty(), "entries of {}", x.ino);
            }
        }
    }

    #[test]
    fn increments_promotion_and_failback_keep_trees_equal() {
        let mut h = H::default();
        h.ok(FsOp::CreateFs {
            fs: "f".into(),
            name: "f".into(),
            now_ns: 1,
            extent_bytes: Some(GRID),
        });
        let d = h.mk("f", ROOT_INO, "d", NodeType::Dir);
        let a = h.mk("f", d, "a", NodeType::File);
        h.write("f", a, 0, "e1", 4096);
        h.write("f", a, GRID, "e2", 100);
        h.mk("f", ROOT_INO, "l", NodeType::Symlink);
        h.mk("f", ROOT_INO, "fifo", NodeType::Fifo);
        let now_ns = h.now();
        h.ok(FsOp::SetXattr {
            fs: "f".into(),
            ino: a,
            name: "user.k".into(),
            value: b"v".to_vec(),
            mode: XattrMode::Set,
            now_ns,
        });
        let now_ns = h.now();
        h.ok(FsOp::Link {
            fs: "f".into(),
            ino: a,
            parent: ROOT_INO,
            name: "a2".into(),
            now_ns,
        });
        h.snapshot("f", "s1");

        h.ok(FsOp::CreateReplica {
            fs: "r".into(),
            name: "r".into(),
            now_ns: 1,
            extent_bytes: GRID,
        });
        h.replicate("r", None, "s1", "r1");
        h.assert_same(&h.c.fs_snapshots["s1"].tree, &h.c.fs_snapshots["r1"].tree);
        h.assert_same(&h.c.fs_snapshots["s1"].tree, h.c.filesystem("r").unwrap());

        // Rewrites, truncation, renames, unlinks, rmdir, chmod and xattr removal.
        let b = h.mk("f", d, "b", NodeType::File);
        h.write("f", b, 0, "e4", 4096);
        h.write("f", b, GRID, "e5", 4096);
        h.write("f", a, GRID, "e3", 200);
        let now_ns = h.now();
        h.ok(FsOp::SetAttr {
            fs: "f".into(),
            ino: b,
            attr: SetAttr {
                size: Some(GRID),
                ..SetAttr::default()
            },
            now_ns,
        });
        let now_ns = h.now();
        h.ok(FsOp::Rename {
            fs: "f".into(),
            parent: d,
            name: "a".into(),
            new_parent: ROOT_INO,
            new_name: "moved".into(),
            now_ns,
        });
        let now_ns = h.now();
        h.ok(FsOp::Unlink {
            fs: "f".into(),
            parent: ROOT_INO,
            name: "a2".into(),
            now_ns,
        });
        let e = h.mk("f", ROOT_INO, "e", NodeType::Dir);
        h.mk("f", e, "gone", NodeType::File);
        let now_ns = h.now();
        h.ok(FsOp::Unlink {
            fs: "f".into(),
            parent: e,
            name: "gone".into(),
            now_ns,
        });
        let now_ns = h.now();
        h.ok(FsOp::Rmdir {
            fs: "f".into(),
            parent: ROOT_INO,
            name: "e".into(),
            now_ns,
        });
        let now_ns = h.now();
        h.ok(FsOp::SetAttr {
            fs: "f".into(),
            ino: d,
            attr: SetAttr {
                mode: Some(0o700),
                ..SetAttr::default()
            },
            now_ns,
        });
        let now_ns = h.now();
        h.ok(FsOp::RemoveXattr {
            fs: "f".into(),
            ino: a,
            name: "user.k".into(),
            now_ns,
        });
        h.snapshot("f", "s2");
        let page = diff_page(&h.c, GRID, "s2", Some("s1"), 0, 100).unwrap();
        let ia = page.inodes.iter().find(|i| i.inode.ino == a).unwrap();
        assert_eq!(ia.data, vec![(GRID, 200)], "only the rewritten cell moves");
        h.replicate("r", Some("s1"), "s2", "r2");
        h.assert_same(&h.c.fs_snapshots["s2"].tree, &h.c.fs_snapshots["r2"].tree);

        // A replica refuses client changes, and its base snapshot can't be deleted.
        let now_ns = h.now();
        let err = h
            .run(FsOp::Unlink {
                fs: "r".into(),
                parent: ROOT_INO,
                name: "moved".into(),
                now_ns,
            })
            .unwrap_err();
        assert!(matches!(err, MetaError::ReadOnly(_)), "{err:?}");
        let err = h
            .run(FsOp::DeleteFsSnapshot { id: "r2".into() })
            .unwrap_err();
        assert!(matches!(err, MetaError::Invalid(_)), "{err:?}");
        // An increment that doesn't start at the base is refused.
        let err = h
            .run(FsOp::ReplicaApply {
                fs: "r".into(),
                from: Some("s1".into()),
                to: "s9".into(),
                inodes: vec![],
                removed: vec![],
            })
            .unwrap_err();
        assert!(matches!(err, MetaError::Invalid(_)), "{err:?}");

        // A partial increment is discarded by promotion.
        h.write("f", b, 0, "e6", 4096);
        h.mk("f", ROOT_INO, "late", NodeType::File);
        h.snapshot("f", "s3");
        let page = diff_page(&h.c, GRID, "s3", Some("s2"), 0, 1).unwrap();
        h.ok(FsOp::ReplicaApply {
            fs: "r".into(),
            from: Some("s2".into()),
            to: "s3".into(),
            inodes: page.inodes.iter().map(|d| d.inode.clone()).collect(),
            removed: page.removed,
        });
        h.ok(FsOp::ReplicaPromote {
            fs: "r".into(),
            force: false,
        });
        assert!(h.c.filesystem("r").unwrap().replica.is_none());
        h.assert_same(&h.c.fs_snapshots["s2"].tree, h.c.filesystem("r").unwrap());

        // Failback: the promoted replica takes writes, the old source becomes its replica at
        // the last common state and catches up.
        let fresh = h.mk("r", ROOT_INO, "written-on-r", NodeType::File);
        assert!(fresh >= h.c.fs_snapshots["s2"].tree.next_ino);
        h.write("r", fresh, 0, "e7", 10);
        h.snapshot("r", "r3");
        h.ok(FsOp::ReplicaDemote {
            fs: "f".into(),
            snapshot: "s2".into(),
            base: "r2".into(),
        });
        h.assert_same(&h.c.fs_snapshots["s2"].tree, h.c.filesystem("f").unwrap());
        let now_ns = h.now();
        let err = h
            .run(FsOp::Mknode {
                fs: "f".into(),
                parent: ROOT_INO,
                name: "x".into(),
                op_id: "x".into(),
                node_type: NodeType::File,
                target: None,
                rdev: 0,
                mode: 0o644,
                create_mode: None,
                uid: 0,
                gid: 0,
                now_ns,
            })
            .unwrap_err();
        assert!(matches!(err, MetaError::ReadOnly(_)), "{err:?}");
        h.replicate("f", Some("r2"), "r3", "f3");
        h.assert_same(&h.c.fs_snapshots["r3"].tree, h.c.filesystem("f").unwrap());
    }

    #[test]
    fn a_new_increment_discards_a_partial_one() {
        let mut h = H::default();
        h.ok(FsOp::CreateFs {
            fs: "f".into(),
            name: "f".into(),
            now_ns: 1,
            extent_bytes: Some(GRID),
        });
        h.mk("f", ROOT_INO, "one", NodeType::File);
        h.snapshot("f", "s1");
        h.ok(FsOp::CreateReplica {
            fs: "r".into(),
            name: "r".into(),
            now_ns: 1,
            extent_bytes: GRID,
        });
        // Part of an increment to s1 arrives, then s1 is gone and s2 starts over from nothing.
        let page = diff_page(&h.c, GRID, "s1", None, 0, 100).unwrap();
        h.ok(FsOp::ReplicaApply {
            fs: "r".into(),
            from: None,
            to: "s1".into(),
            inodes: page.inodes.iter().map(|d| d.inode.clone()).collect(),
            removed: vec![],
        });
        let now_ns = h.now();
        h.ok(FsOp::Unlink {
            fs: "f".into(),
            parent: ROOT_INO,
            name: "one".into(),
            now_ns,
        });
        h.mk("f", ROOT_INO, "two", NodeType::File);
        h.snapshot("f", "s2");
        h.replicate("r", None, "s2", "r2");
        h.assert_same(&h.c.fs_snapshots["s2"].tree, &h.c.fs_snapshots["r2"].tree);
        // Promoting a replica with no complete increment needs force.
        h.ok(FsOp::CreateReplica {
            fs: "q".into(),
            name: "q".into(),
            now_ns: 1,
            extent_bytes: GRID,
        });
        let err = h
            .run(FsOp::ReplicaPromote {
                fs: "q".into(),
                force: false,
            })
            .unwrap_err();
        assert!(matches!(err, MetaError::Invalid(_)), "{err:?}");
        h.ok(FsOp::ReplicaPromote {
            fs: "q".into(),
            force: true,
        });
    }
}
