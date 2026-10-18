// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! File and directory operations over the namespace in the catalog. File data goes through the
//! same extent grid, replication and copy-on-write path as volume data.

use serde::{Deserialize, Serialize};

use super::{now_ns, NativeEngine, NativeError, Target};
use crate::{
    metadata::{Catalog, MetaCommand, MetaError, SnapshotId},
    namespace::{
        FsId, FsMeta, FsOp, FsQuota, Inode, InodeKind, NodeType, ReplicaState, SetAttr, XattrMode,
    },
};

/// File extent grid for new filesystems: a 4 KiB write rewrites at most this much.
pub const DEFAULT_FS_EXTENT_BYTES: u64 = 1 << 20;

/// What `stat(2)` needs about an inode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attr {
    pub ino: u64,
    pub kind: NodeType,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub size: u64,
    /// Device number of a character or block device node.
    #[serde(default)]
    pub rdev: u64,
    /// Allocated 512-byte blocks (holes take none).
    pub blocks: u64,
    pub atime_ns: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

/// The extents behind a byte range of a file; see [`NativeEngine::file_layout`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileLayout {
    pub offset: u64,
    /// The requested length clipped to end of file; bytes not covered by an extent are zeros.
    pub len: usize,
    pub size: u64,
    pub extents: Vec<LayoutExtent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutExtent {
    pub logical_offset: u64,
    pub len: usize,
    /// Hex SHA-256 of the whole extent.
    pub checksum: String,
    /// Full copies in the order to try them; for an erasure-coded extent, its shards in shard
    /// order.
    pub replicas: Vec<LayoutReplica>,
    /// Set for an erasure-coded extent; a client that doesn't know it falls back on a checksum
    /// mismatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ec: Option<LayoutEc>,
}

/// How to read an erasure-coded extent directly: concatenate data shards `0..data` (each
/// `shard_len` bytes, verified against its hex SHA-256) and cut to the extent's length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutEc {
    pub data: usize,
    pub parity: usize,
    pub shard_len: usize,
    pub shard_checksums: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutReplica {
    pub node_id: String,
    /// The data node's address, absent for a local device or an unhealthy node.
    pub endpoint: Option<String>,
    /// Which of the data node's devices holds the replica.
    #[serde(default)]
    pub device_index: usize,
    pub offset: u64,
    /// The data node's host (failure domain), so a client can prefer replicas on its own host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub ino: u64,
    pub kind: NodeType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsInfo {
    pub id: FsId,
    pub name: String,
    pub inodes: usize,
    /// Sum of file sizes.
    pub bytes: u64,
    pub source_snapshot: Option<SnapshotId>,
    /// File extent grid: the most a small write rewrites.
    pub extent_bytes: u64,
    /// Set on a replica ([`ReplicaState`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replica: Option<ReplicaState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsSnapshotInfo {
    pub id: SnapshotId,
    pub fs_id: FsId,
    pub name: String,
    pub created_ns: i64,
    pub inodes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsStat {
    pub inodes: u64,
    pub used_bytes: u64,
    /// Device bytes (summed over replicas) on the free lists.
    pub free_list_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<FsQuota>,
}

/// A directory entry to create.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewNode {
    pub name: String,
    /// Client-chosen id: retrying the same create returns the inode it made.
    pub op_id: String,
    pub kind: NodeType,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub rdev: u64,
    #[serde(default)]
    pub mode: u32,
    /// The mode before the umask, for a parent with a default ACL (`FsOp::Mknode`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_mode: Option<u32>,
    #[serde(default)]
    pub uid: u32,
    #[serde(default)]
    pub gid: u32,
}

fn kind_of(i: &Inode) -> NodeType {
    match i.kind {
        InodeKind::Dir { .. } => NodeType::Dir,
        InodeKind::File { .. } => NodeType::File,
        InodeKind::Symlink { .. } => NodeType::Symlink,
        InodeKind::Special { node_type, .. } => node_type,
    }
}

fn attr(c: &Catalog, i: &Inode) -> Attr {
    let allocated: u64 = match &i.kind {
        InodeKind::File { extents, .. } => extents
            .values()
            .filter_map(|e| c.extents.get(e))
            .map(|m| m.extent.len as u64)
            .sum(),
        _ => 0,
    };
    Attr {
        ino: i.ino,
        kind: kind_of(i),
        mode: i.mode,
        uid: i.uid,
        gid: i.gid,
        nlink: i.nlink,
        size: i.size(),
        rdev: match i.kind {
            InodeKind::Special { rdev, .. } => rdev,
            _ => 0,
        },
        blocks: allocated.div_ceil(512),
        atime_ns: i.atime_ns,
        mtime_ns: i.mtime_ns,
        ctime_ns: i.ctime_ns,
    }
}

/// Snapshot views (`fs@snap`) only serve reads.
fn writable(fs: &str) -> Result<(), NativeError> {
    if fs.contains('@') {
        return Err(NativeError::ReadOnly(format!("{fs} is a snapshot")));
    }
    Ok(())
}

impl NativeEngine {
    fn fs_commit(&self, op: FsOp) -> Result<(), NativeError> {
        self.commit(MetaCommand::Fs { op }, None)
    }

    pub(super) fn with_fs<R>(
        &self,
        fs: &str,
        f: impl FnOnce(&Catalog, &FsMeta) -> Result<R, NativeError>,
    ) -> Result<R, NativeError> {
        self.with_catalog(|c| f(c, c.fs_view(fs)?))?
    }

    /// Creates an empty filesystem with a caller-chosen id (idempotent) and the default grid.
    pub fn create_fs_as(&self, id: String, name: impl Into<String>) -> Result<FsId, NativeError> {
        self.create_fs_with(id, name, None)
    }

    /// As [`Self::create_fs_as`] with an explicit file extent grid, at most the cluster's
    /// `extent_bytes`. `None` picks [`DEFAULT_FS_EXTENT_BYTES`] (or the cluster grid if that is
    /// smaller).
    pub fn create_fs_with(
        &self,
        id: String,
        name: impl Into<String>,
        extent_bytes: Option<u64>,
    ) -> Result<FsId, NativeError> {
        writable(&id)?;
        let cluster = self.cfg.extent_bytes as u64;
        let grid = extent_bytes.unwrap_or(DEFAULT_FS_EXTENT_BYTES.min(cluster));
        if grid == 0 || grid > cluster {
            return Err(NativeError::Invalid(format!(
                "extent_bytes must be 1..={cluster}, got {grid}"
            )));
        }
        self.fs_commit(FsOp::CreateFs {
            fs: id.clone(),
            name: name.into(),
            now_ns: now_ns(),
            extent_bytes: Some(grid),
        })?;
        Ok(id)
    }

    pub fn delete_fs(&self, fs: &str) -> Result<(), NativeError> {
        writable(fs)?;
        self.fs_commit(FsOp::DeleteFs { fs: fs.into() })
    }

    pub fn filesystems(&self) -> Result<Vec<FsInfo>, NativeError> {
        self.with_catalog(|c| {
            c.filesystems
                .values()
                .map(|f| FsInfo {
                    id: f.id.clone(),
                    name: f.name.clone(),
                    inodes: f.inodes.len() as usize,
                    bytes: f.usage.unwrap_or_default().file_bytes,
                    source_snapshot: f.source_snapshot.clone(),
                    extent_bytes: f.extent_bytes.unwrap_or(self.cfg.extent_bytes as u64),
                    replica: f.replica.clone(),
                })
                .collect()
        })
    }

    pub fn fs_statfs(&self, fs: &str) -> Result<FsStat, NativeError> {
        self.with_fs(fs, |c, f| {
            Ok(FsStat {
                inodes: f.inodes.len(),
                used_bytes: f.usage.unwrap_or_default().used_bytes,
                free_list_bytes: c.free.total_bytes(),
                quota: f.quota,
            })
        })
    }

    /// The filesystem's quota (also readable on a snapshot, which keeps the one it was taken with).
    pub fn fs_quota(&self, fs: &str) -> Result<Option<FsQuota>, NativeError> {
        self.with_fs(fs, |_, f| Ok(f.quota))
    }

    /// Replaces the filesystem's quota; [`FsQuota::default`] removes it. A limit below current
    /// usage is accepted: it blocks further growth until usage falls under it.
    pub fn fs_set_quota(&self, fs: &str, quota: FsQuota) -> Result<(), NativeError> {
        writable(fs)?;
        self.fs_commit(FsOp::SetQuota {
            fs: fs.into(),
            max_bytes: quota.max_bytes,
            max_inodes: quota.max_inodes,
        })
    }

    pub fn fs_getattr(&self, fs: &str, ino: u64) -> Result<Attr, NativeError> {
        self.with_fs(fs, |c, f| Ok(attr(c, f.inode(ino)?.as_ref())))
    }

    /// `name` in directory `parent`; `.` is the directory itself and `..` its parent (the root's
    /// is the root), for clients reconnecting a file handle by inode number.
    pub fn fs_lookup(&self, fs: &str, parent: u64, name: &str) -> Result<Attr, NativeError> {
        self.with_fs(fs, |c, f| {
            let ino = match name {
                "." => f.dir(parent)?.ino,
                ".." => match f.dir(parent)?.kind {
                    InodeKind::Dir { parent, .. } => parent,
                    _ => unreachable!("dir() returns directories"),
                },
                _ => f.lookup(parent, name)?,
            };
            Ok(attr(c, f.inode(ino)?.as_ref()))
        })
    }

    /// Every entry of a directory.
    pub fn fs_readdir(&self, fs: &str, dir: u64) -> Result<Vec<DirEntry>, NativeError> {
        self.fs_readdir_page(fs, dir, None, usize::MAX)
    }

    /// Up to `limit` entries of a directory named after `after`, in name order.
    pub fn fs_readdir_page(
        &self,
        fs: &str,
        dir: u64,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DirEntry>, NativeError> {
        self.with_fs(fs, |_, f| {
            f.entries(dir, after, limit)?
                .into_iter()
                .map(|(name, ino)| {
                    Ok(DirEntry {
                        name,
                        ino,
                        kind: kind_of(f.inode(ino)?.as_ref()),
                    })
                })
                .collect()
        })
    }

    pub fn fs_readlink(&self, fs: &str, ino: u64) -> Result<String, NativeError> {
        self.with_fs(fs, |_, f| match &f.inode(ino)?.kind {
            InodeKind::Symlink { target } => Ok(target.clone()),
            _ => Err(NativeError::Invalid(format!(
                "inode {ino} is not a symlink"
            ))),
        })
    }

    /// Names of the inode's extended attributes.
    pub fn fs_listxattr(&self, fs: &str, ino: u64) -> Result<Vec<String>, NativeError> {
        self.with_fs(fs, |_, f| {
            Ok(f.inode(ino)?.xattrs.keys().cloned().collect())
        })
    }

    pub fn fs_getxattr(&self, fs: &str, ino: u64, name: &str) -> Result<Vec<u8>, NativeError> {
        self.with_fs(fs, |_, f| {
            f.inode(ino)?
                .xattrs
                .get(name)
                .cloned()
                .ok_or_else(|| MetaError::NoAttr(name.into()).into())
        })
    }

    pub fn fs_setxattr(
        &self,
        fs: &str,
        ino: u64,
        name: &str,
        value: &[u8],
        mode: XattrMode,
    ) -> Result<(), NativeError> {
        writable(fs)?;
        self.fs_commit(FsOp::SetXattr {
            fs: fs.into(),
            ino,
            name: name.into(),
            value: value.to_vec(),
            mode,
            now_ns: now_ns(),
        })
    }

    pub fn fs_removexattr(&self, fs: &str, ino: u64, name: &str) -> Result<(), NativeError> {
        writable(fs)?;
        self.fs_commit(FsOp::RemoveXattr {
            fs: fs.into(),
            ino,
            name: name.into(),
            now_ns: now_ns(),
        })
    }

    pub fn fs_mknode(&self, fs: &str, parent: u64, node: NewNode) -> Result<Attr, NativeError> {
        writable(fs)?;
        let name = node.name.clone();
        self.fs_commit(FsOp::Mknode {
            fs: fs.into(),
            parent,
            name: node.name,
            op_id: node.op_id,
            node_type: node.kind,
            target: node.target,
            rdev: node.rdev,
            mode: node.mode,
            create_mode: node.create_mode,
            uid: node.uid,
            gid: node.gid,
            now_ns: now_ns(),
        })?;
        self.fs_lookup(fs, parent, &name)
    }

    pub fn fs_link(
        &self,
        fs: &str,
        ino: u64,
        parent: u64,
        name: &str,
    ) -> Result<Attr, NativeError> {
        writable(fs)?;
        self.fs_commit(FsOp::Link {
            fs: fs.into(),
            ino,
            parent,
            name: name.into(),
            now_ns: now_ns(),
        })?;
        self.fs_getattr(fs, ino)
    }

    pub fn fs_unlink(&self, fs: &str, parent: u64, name: &str) -> Result<(), NativeError> {
        writable(fs)?;
        self.fs_commit(FsOp::Unlink {
            fs: fs.into(),
            parent,
            name: name.into(),
            now_ns: now_ns(),
        })?;
        self.fs_activity.record_delete(fs);
        Ok(())
    }

    pub fn fs_rmdir(&self, fs: &str, parent: u64, name: &str) -> Result<(), NativeError> {
        writable(fs)?;
        self.fs_commit(FsOp::Rmdir {
            fs: fs.into(),
            parent,
            name: name.into(),
            now_ns: now_ns(),
        })?;
        self.fs_activity.record_delete(fs);
        Ok(())
    }

    pub fn fs_rename(
        &self,
        fs: &str,
        parent: u64,
        name: &str,
        new_parent: u64,
        new_name: &str,
    ) -> Result<(), NativeError> {
        writable(fs)?;
        self.fs_commit(FsOp::Rename {
            fs: fs.into(),
            parent,
            name: name.into(),
            new_parent,
            new_name: new_name.into(),
            now_ns: now_ns(),
        })?;
        self.fs_activity.record_rename(fs);
        Ok(())
    }

    /// Changes attributes. Truncating into the middle of an extent first rewrites that extent
    /// without its tail, so growing the file again later reads zeros there, not stale bytes.
    pub fn fs_setattr(&self, fs: &str, ino: u64, attr: SetAttr) -> Result<Attr, NativeError> {
        writable(fs)?;
        let _write = self
            .write_lock
            .lock()
            .map_err(|_| NativeError::Poisoned("write"))?;
        if let Some(size) = attr.size {
            let target = Target::File {
                fs,
                ino,
                replica: false,
            };
            let grid = self.with_catalog(|c| target.grid(c, self.cfg.extent_bytes))??;
            let cell = size - size % grid;
            if size > cell {
                if let Some(ext) = self.with_catalog(|c| target.extent_at(c, cell))?? {
                    if cell + ext.len as u64 > size {
                        let fence = self.write_fence()?;
                        let mut buf = self.read_extent(&ext)?;
                        buf.truncate((size - cell) as usize);
                        self.install_extent(&target, cell, &buf, fence)?;
                    }
                }
            }
        }
        self.fs_commit(FsOp::SetAttr {
            fs: fs.into(),
            ino,
            attr,
            now_ns: now_ns(),
        })?;
        self.fs_getattr(fs, ino)
    }

    /// Writes `data` at `offset`, growing the file as needed; a gap past the old end is a hole.
    pub fn write_file(
        &self,
        fs: &str,
        ino: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<Attr, NativeError> {
        self.write_file_as(fs, ino, offset, data, false)
    }

    /// [`Self::write_file`] of replication data into a replica: the inode's times stay as the
    /// increment set them.
    pub fn replica_write(
        &self,
        fs: &str,
        ino: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<Attr, NativeError> {
        self.write_file_as(fs, ino, offset, data, true)
    }

    fn write_file_as(
        &self,
        fs: &str,
        ino: u64,
        offset: u64,
        data: &[u8],
        replica: bool,
    ) -> Result<Attr, NativeError> {
        writable(fs)?;
        offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| NativeError::Invalid("write offset overflows".into()))?;
        let target = Target::File { fs, ino, replica };
        if data.is_empty() {
            self.with_catalog(|c| target.extent_at(c, 0))??;
        } else if !self.write_aligned(&target, offset, data, |c| {
            target.extent_at(c, 0).map(|_| ())
        })? {
            let _write = self
                .write_lock
                .lock()
                .map_err(|_| NativeError::Poisoned("write"))?;
            let fence = self.write_fence()?;
            self.write_locked(&target, offset, data, fence)?;
        }
        if !replica {
            self.fs_activity.record_write(fs, ino, data);
        }
        self.fs_getattr(fs, ino)
    }

    /// Reads up to `len` bytes at `offset`; short at end of file, holes read as zeros.
    pub fn read_file(
        &self,
        fs: &str,
        ino: u64,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, NativeError> {
        let (extents, len) = self.with_fs(fs, |c, f| {
            let inode = f.inode(ino)?;
            let InodeKind::File { size, extents } = &inode.kind else {
                return Err(NativeError::Invalid(format!(
                    "inode {ino} is not a regular file"
                )));
            };
            let len = (*size).saturating_sub(offset).min(len as u64) as usize;
            if len == 0 {
                return Ok((Vec::new(), 0));
            }
            let grid = f.extent_bytes.unwrap_or(self.cfg.extent_bytes as u64);
            Ok((self.extents_in(c, extents, grid, *size, offset, len)?, len))
        })?;
        self.read_range(extents, offset, len)
    }

    /// Where the bytes [`Self::read_file`] would return live: each overlapping extent with its
    /// checksum and replicas (in the order a read should try them), so a client can fetch them
    /// from the data nodes itself. Replicas on local devices carry no endpoint.
    pub fn file_layout(
        &self,
        fs: &str,
        ino: u64,
        offset: u64,
        len: usize,
    ) -> Result<FileLayout, NativeError> {
        let (extents, len, size) = self.with_fs(fs, |c, f| {
            let inode = f.inode(ino)?;
            let InodeKind::File { size, extents } = &inode.kind else {
                return Err(NativeError::Invalid(format!(
                    "inode {ino} is not a regular file"
                )));
            };
            let len = (*size).saturating_sub(offset).min(len as u64) as usize;
            if len == 0 {
                return Ok((Vec::new(), 0, *size));
            }
            let grid = f.extent_bytes.unwrap_or(self.cfg.extent_bytes as u64);
            Ok((
                self.extents_in(c, extents, grid, *size, offset, len)?,
                len,
                *size,
            ))
        })?;
        let extents = extents
            .into_iter()
            .map(|ext| {
                let start = match ext.ec {
                    Some(_) => 0,
                    None => super::replica_start(&ext.id, ext.replicas.len()),
                };
                let replicas = ext
                    .replicas
                    .iter()
                    .cycle()
                    .skip(start)
                    .take(ext.replicas.len())
                    .map(|r| LayoutReplica {
                        node_id: r.node_id.clone(),
                        endpoint: self
                            .nodes
                            .iter()
                            .find(|n| n.spec.id == r.node_id && n.spec.healthy)
                            .and_then(|n| n.devices.get(r.device_index))
                            .and_then(|d| d.endpoint().map(str::to_string)),
                        device_index: r.device_index,
                        offset: r.offset,
                        host: self
                            .node(&r.node_id)
                            .ok()
                            .map(|n| n.spec.failure_domain.host.clone()),
                    })
                    .collect();
                LayoutExtent {
                    logical_offset: ext.logical_offset,
                    len: ext.len,
                    checksum: hex(&ext.checksum),
                    replicas,
                    ec: ext.ec.as_ref().map(|e| LayoutEc {
                        data: e.data,
                        parity: e.parity,
                        shard_len: e.shard_len,
                        shard_checksums: e.shard_checksums.iter().map(|c| hex(c)).collect(),
                    }),
                }
            })
            .collect();
        Ok(FileLayout {
            offset,
            len,
            size,
            extents,
        })
    }

    /// Freezes `fs` as a read-only snapshot (metadata only), with a caller-chosen id.
    pub fn snapshot_fs_as(
        &self,
        id: String,
        fs: &str,
        name: impl Into<String>,
    ) -> Result<SnapshotId, NativeError> {
        writable(fs)?;
        self.fs_commit(FsOp::SnapshotFs {
            id: id.clone(),
            fs: fs.into(),
            name: name.into(),
            now_ns: now_ns(),
        })?;
        Ok(id)
    }

    pub fn delete_fs_snapshot(&self, id: &str) -> Result<(), NativeError> {
        self.fs_commit(FsOp::DeleteFsSnapshot {
            id: id.into(),
            now_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        })
    }

    /// Retention-locks a filesystem snapshot for `retain_secs` from now; returns the unlock time.
    pub fn lock_fs_snapshot(&self, id: &str, retain_secs: u64) -> Result<u64, NativeError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let retain_until_unix = now.saturating_add(retain_secs);
        self.fs_commit(FsOp::LockFsSnapshot {
            id: id.into(),
            retain_until_unix,
        })?;
        Ok(retain_until_unix)
    }

    /// A writable copy-on-write filesystem from a snapshot, with a caller-chosen id.
    pub fn clone_fs_as(
        &self,
        id: String,
        snapshot_id: &str,
        name: impl Into<String>,
    ) -> Result<FsId, NativeError> {
        writable(&id)?;
        self.fs_commit(FsOp::CloneFs {
            id: id.clone(),
            name: name.into(),
            snapshot_id: snapshot_id.into(),
        })?;
        Ok(id)
    }

    pub fn fs_snapshots(&self) -> Result<Vec<FsSnapshotInfo>, NativeError> {
        self.with_catalog(|c| {
            c.fs_snapshots
                .values()
                .map(|s| FsSnapshotInfo {
                    id: s.id.clone(),
                    fs_id: s.fs_id.clone(),
                    name: s.name.clone(),
                    created_ns: s.created_ns,
                    inodes: s.tree.inodes.len() as usize,
                })
                .collect()
        })
    }
}
