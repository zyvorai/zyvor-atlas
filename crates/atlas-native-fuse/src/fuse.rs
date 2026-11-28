// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The kernel side: a `fuser` filesystem that forwards every request to [`Ops`].

use std::{
    ffi::OsStr,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use atlas_native::{
    engine::{Attr, FsStat, NewNode},
    NodeType, SetAttr,
};
use fuser::{
    AccessFlags, BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, OpenAccMode, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLock,
    ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};

use crate::{
    dirs::DirStreams,
    invalidate::Invalidator,
    locks::F_UNLCK,
    ops::{Ops, WriteMode},
};

const STATFS_BLOCK: u64 = 4096;

#[derive(Debug, PartialEq, Eq)]
struct StatfsCounts {
    blocks: u64,
    bfree: u64,
    files: u64,
    ffree: u64,
}

/// `statfs` totals: a quota's limits when one is set (so `df` shows the quota as the size),
/// otherwise what is used plus headroom, since capacity is the cluster's, not this filesystem's.
fn statfs_counts(s: &FsStat) -> StatfsCounts {
    const HEADROOM_BLOCKS: u64 = 1 << 28;
    const HEADROOM_INODES: u64 = 1 << 24;
    let used_blocks = s.used_bytes.div_ceil(STATFS_BLOCK);
    let quota = s.quota.unwrap_or_default();
    let (blocks, bfree) = match quota.max_bytes {
        Some(max) => {
            let total = max / STATFS_BLOCK;
            (total, total.saturating_sub(used_blocks))
        }
        None => (used_blocks + HEADROOM_BLOCKS, HEADROOM_BLOCKS),
    };
    let (files, ffree) = match quota.max_inodes {
        Some(max) => (max, max.saturating_sub(s.inodes)),
        None => (s.inodes + HEADROOM_INODES, HEADROOM_INODES),
    };
    StatfsCounts {
        blocks,
        bfree,
        files,
        ffree,
    }
}

pub struct AtlasFs {
    pub ops: Ops,
    dirs: DirStreams,
    posix_acl: bool,
    /// Drops the kernel's cached copy of files that lost buffered writes.
    invalidator: Option<Invalidator>,
}

fn time(ns: i64) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    } else {
        UNIX_EPOCH
    }
}

fn ns(t: TimeOrNow) -> i64 {
    let t = match t {
        TimeOrNow::SpecificTime(t) => t,
        TimeOrNow::Now => SystemTime::now(),
    };
    t.duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn file_attr(a: &Attr) -> FileAttr {
    FileAttr {
        ino: INodeNo(a.ino),
        size: a.size,
        blocks: a.blocks,
        atime: time(a.atime_ns),
        mtime: time(a.mtime_ns),
        ctime: time(a.ctime_ns),
        crtime: time(a.ctime_ns),
        kind: kind(a.kind),
        perm: (a.mode & 0o7777) as u16,
        nlink: a.nlink,
        uid: a.uid,
        gid: a.gid,
        rdev: a.rdev as u32,
        blksize: 4096,
        flags: 0,
    }
}

fn kind(k: NodeType) -> FileType {
    match k {
        NodeType::File => FileType::RegularFile,
        NodeType::Dir => FileType::Directory,
        NodeType::Symlink => FileType::Symlink,
        NodeType::Fifo => FileType::NamedPipe,
        NodeType::Socket => FileType::Socket,
        NodeType::CharDevice => FileType::CharDevice,
        NodeType::BlockDevice => FileType::BlockDevice,
    }
}

fn err(e: i32) -> Errno {
    Errno::from_i32(e)
}

fn name(n: &OsStr) -> Result<&str, Errno> {
    n.to_str().ok_or(Errno::EINVAL)
}

/// File handle bit: opened with `O_DIRECT`. The other bits are the [`Ops`] handle.
const FH_DIRECT: u64 = 1;

/// The [`Ops`] handle in a FUSE file handle.
fn ops_handle(fh: FileHandle) -> u64 {
    fh.0 >> 1
}

/// The file handle and open flags for a file opened with `flags` as [`Ops`] handle `ops`:
/// `O_DIRECT` bypasses the kernel's page cache (`FOPEN_DIRECT_IO`, with parallel writes to one
/// file allowed) and this client's write-back and read-ahead.
fn handle(ops: u64, flags: i32) -> (FileHandle, FopenFlags) {
    if flags & libc::O_DIRECT != 0 {
        (
            FileHandle(ops << 1 | FH_DIRECT),
            FopenFlags::FOPEN_DIRECT_IO | FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES,
        )
    } else {
        (FileHandle(ops << 1), FopenFlags::empty())
    }
}

/// Whether a handle opened with `flags` can write.
fn writable(flags: i32) -> bool {
    flags & libc::O_ACCMODE != libc::O_RDONLY
}

/// A node to create with the umask applied, and the mode before it for a parent with a default
/// ACL (with `--acl` the kernel leaves masking to us: `FUSE_DONT_MASK`).
fn new_node(req: &Request, n: &str, kind: NodeType, mode: u32, umask: u32) -> NewNode {
    NewNode {
        name: n.into(),
        op_id: String::new(),
        kind,
        target: None,
        rdev: 0,
        mode: mode & !umask,
        create_mode: Some(mode),
        uid: req.uid(),
        gid: req.gid(),
    }
}

/// `size == 0` asks for the length; a buffer too small for the value is ERANGE.
fn xattr_reply(reply: ReplyXattr, value: &[u8], size: u32) {
    if size == 0 {
        reply.size(value.len() as u32);
    } else if value.len() > size as usize {
        reply.error(Errno::ERANGE);
    } else {
        reply.data(value);
    }
}

impl AtlasFs {
    pub fn new(ops: Ops) -> Self {
        Self {
            ops,
            dirs: DirStreams::default(),
            posix_acl: false,
            invalidator: None,
        }
    }

    /// Drops the kernel's cached copy of a file whose buffered writes are lost, and does so
    /// before replying to an fsync or close that reports it. The caller serves the queue of
    /// `invalidator` with the session's notifier.
    pub fn with_invalidation(mut self, invalidator: Invalidator) -> Self {
        let lost = invalidator.clone();
        self.ops.on_lost_writes(move |ino| lost.lost(ino));
        self.invalidator = Some(invalidator);
        self
    }

    /// Replies to an fsync or close of `ino`.
    fn report(&self, ino: u64, result: Result<(), i32>, reply: ReplyEmpty) {
        match (result, &self.invalidator) {
            (Ok(()), _) => reply.ok(),
            (Err(e), Some(i)) => i.then(ino, move || reply.error(err(e))),
            (Err(e), None) => reply.error(err(e)),
        }
    }

    /// Has the kernel enforce POSIX ACLs (`FUSE_POSIX_ACL`); the mount fails on a kernel
    /// without it rather than silently ignoring ACLs.
    pub fn with_posix_acl(mut self, on: bool) -> Self {
        self.posix_acl = on;
        self
    }

    fn ttl(&self) -> Duration {
        self.ops.kernel_ttl()
    }

    fn entry(&self, r: Result<Attr, i32>, reply: ReplyEntry) {
        match r {
            Ok(a) => reply.entry(&self.ttl(), &file_attr(&a), Generation(0)),
            Err(e) => reply.error(err(e)),
        }
    }

    fn mutate(&self) -> Result<(), Errno> {
        if self.ops.read_only() {
            return Err(Errno::EROFS);
        }
        Ok(())
    }
}

macro_rules! tri {
    ($reply:ident, $e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return $reply.error(e),
        }
    };
}

impl Filesystem for AtlasFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // The kernel's default read-ahead (128 KiB) would cap sequential reads far below what
        // the client's own read-ahead window can serve.
        if let Err(max) = config.set_max_readahead(16 << 20) {
            let _ = config.set_max_readahead(max);
        }
        // Send fcntl and flock locks here so every mount sees them; a kernel without these
        // keeps them local to the mount.
        for cap in [InitFlags::FUSE_POSIX_LOCKS, InitFlags::FUSE_FLOCK_LOCKS] {
            if config.add_capabilities(cap).is_err() {
                tracing::warn!(?cap, "kernel lacks the capability; those locks stay local");
            }
        }
        // File handles name inodes by number (never reused, so generation 0): the kernel
        // resolves one it no longer caches by looking up `.` and `..`, which lets NFS servers
        // re-export the mount.
        if config
            .add_capabilities(InitFlags::FUSE_EXPORT_SUPPORT)
            .is_err()
        {
            tracing::warn!("kernel lacks FUSE_EXPORT_SUPPORT; the mount cannot be re-exported");
        }
        if self.posix_acl {
            config
                .add_capabilities(InitFlags::FUSE_POSIX_ACL | InitFlags::FUSE_DONT_MASK)
                .map_err(|missing| {
                    std::io::Error::other(format!(
                        "--acl: the kernel lacks {missing:?}; ACLs would not be enforced"
                    ))
                })?;
        }
        Ok(())
    }

    fn destroy(&mut self) {
        if let Err(e) = self.ops.flush_all() {
            tracing::error!(errno = e, "buffered writes lost at unmount");
        }
        self.ops.close_session();
    }

    fn getlk(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        match self.ops.getlk(ino.0, lock_owner.0, start, end, typ) {
            Ok(Some(c)) => reply.locked(c.start, c.end, c.typ, c.pid),
            Ok(None) => reply.locked(start, end, F_UNLCK, pid),
            Err(e) => reply.error(err(e)),
        }
    }

    fn setlk(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        sleep: bool,
        reply: ReplyEmpty,
    ) {
        match self
            .ops
            .setlk(ino.0, lock_owner.0, start, end, typ, pid, sleep)
        {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, n: &OsStr, reply: ReplyEntry) {
        let n = tri!(reply, name(n));
        let r = match n {
            "." | ".." => self.ops.lookup_dot(parent.0, n),
            _ => self.ops.lookup(parent.0, n),
        };
        self.entry(r, reply);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.ops.getattr(ino.0) {
            Ok(a) => reply.attr(&self.ttl(), &file_attr(&a)),
            Err(e) => reply.error(err(e)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        tri!(reply, self.mutate());
        let attr = SetAttr {
            mode,
            uid,
            gid,
            size,
            atime_ns: atime.map(ns),
            mtime_ns: mtime.map(ns),
        };
        match self.ops.setattr(ino.0, attr) {
            Ok(a) => reply.attr(&self.ttl(), &file_attr(&a)),
            Err(e) => reply.error(err(e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.ops.readlink(ino.0) {
            Ok(t) => reply.data(t.as_bytes()),
            Err(e) => reply.error(err(e)),
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        n: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        tri!(reply, self.mutate());
        if position != 0 {
            return reply.error(Errno::EINVAL);
        }
        let n = tri!(reply, name(n));
        let create = flags & libc::XATTR_CREATE != 0;
        let replace = flags & libc::XATTR_REPLACE != 0;
        match self.ops.setxattr(ino.0, n, value, create, replace) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, n: &OsStr, size: u32, reply: ReplyXattr) {
        let n = tri!(reply, name(n));
        match self.ops.getxattr(ino.0, n) {
            Ok(v) => xattr_reply(reply, &v, size),
            Err(e) => reply.error(err(e)),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        match self.ops.listxattr(ino.0) {
            Ok(names) => {
                let mut buf = Vec::new();
                for n in names {
                    buf.extend_from_slice(n.as_bytes());
                    buf.push(0);
                }
                xattr_reply(reply, &buf, size);
            }
            Err(e) => reply.error(err(e)),
        }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, n: &OsStr, reply: ReplyEmpty) {
        tri!(reply, self.mutate());
        let n = tri!(reply, name(n));
        match self.ops.removexattr(ino.0, n) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        n: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        tri!(reply, self.mutate());
        let n = tri!(reply, name(n));
        let kind = match mode & libc::S_IFMT {
            0 | libc::S_IFREG => NodeType::File,
            libc::S_IFIFO => NodeType::Fifo,
            libc::S_IFSOCK => NodeType::Socket,
            libc::S_IFCHR => NodeType::CharDevice,
            libc::S_IFBLK => NodeType::BlockDevice,
            _ => return reply.error(Errno::EINVAL),
        };
        let node = NewNode {
            name: n.into(),
            op_id: String::new(),
            kind,
            target: None,
            rdev: u64::from(rdev),
            mode: mode & !umask,
            create_mode: Some(mode),
            uid: req.uid(),
            gid: req.gid(),
        };
        self.entry(self.ops.create(parent.0, node), reply);
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        n: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        tri!(reply, self.mutate());
        let n = tri!(reply, name(n));
        let r = self
            .ops
            .create(parent.0, new_node(req, n, NodeType::Dir, mode, umask));
        self.entry(r, reply);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, n: &OsStr, reply: ReplyEmpty) {
        tri!(reply, self.mutate());
        let n = tri!(reply, name(n));
        match self.ops.unlink(parent.0, n) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, n: &OsStr, reply: ReplyEmpty) {
        tri!(reply, self.mutate());
        let n = tri!(reply, name(n));
        match self.ops.rmdir(parent.0, n) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn symlink(&self, req: &Request, parent: INodeNo, n: &OsStr, target: &Path, reply: ReplyEntry) {
        tri!(reply, self.mutate());
        let n = tri!(reply, name(n));
        let target = tri!(reply, target.to_str().ok_or(Errno::EINVAL));
        let r = self.ops.mknode(
            parent.0,
            n,
            NodeType::Symlink,
            Some(target.to_string()),
            0o777,
            req.uid(),
            req.gid(),
        );
        self.entry(r, reply);
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        n: &OsStr,
        new_parent: INodeNo,
        new_name: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        tri!(reply, self.mutate());
        // RENAME_NOREPLACE / RENAME_EXCHANGE are not supported.
        if !flags.is_empty() {
            return reply.error(Errno::EINVAL);
        }
        let (n, nn) = (tri!(reply, name(n)), tri!(reply, name(new_name)));
        match self.ops.rename(parent.0, n, new_parent.0, nn) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        new_parent: INodeNo,
        new_name: &OsStr,
        reply: ReplyEntry,
    ) {
        tri!(reply, self.mutate());
        let nn = tri!(reply, name(new_name));
        self.entry(self.ops.link(ino.0, new_parent.0, nn), reply);
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if self.ops.read_only() && flags.acc_mode() != OpenAccMode::O_RDONLY {
            return reply.error(Errno::EROFS);
        }
        match self.ops.open(ino.0, writable(flags.0)) {
            Ok(h) => {
                let (fh, open_flags) = handle(h, flags.0);
                reply.opened(fh, open_flags)
            }
            Err(e) => reply.error(err(e)),
        }
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        n: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        tri!(reply, self.mutate());
        let n = tri!(reply, name(n));
        let created = self
            .ops
            .create(parent.0, new_node(req, n, NodeType::File, mode, umask))
            .and_then(|a| Ok((self.ops.open(a.ino, writable(flags))?, a)));
        match created {
            Ok((h, a)) => {
                let (fh, open_flags) = handle(h, flags);
                reply.created(&self.ttl(), &file_attr(&a), Generation(0), fh, open_flags)
            }
            Err(e) => reply.error(err(e)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let r = if fh.0 & FH_DIRECT != 0 {
            self.ops.read_direct(ino.0, offset, size as usize)
        } else {
            self.ops.read(ino.0, offset, size as usize)
        };
        match r {
            Ok(b) => reply.data(&b),
            Err(e) => reply.error(err(e)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let mode = if write_flags.contains(WriteFlags::FUSE_WRITE_CACHE) {
            WriteMode::Mapped
        } else if fh.0 & FH_DIRECT != 0 {
            WriteMode::Direct
        } else {
            WriteMode::Buffered
        };
        match self.ops.handle_write(ops_handle(fh), offset, data, mode) {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(err(e)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // A process's POSIX locks on a file go with any close of it.
        if let Err(e) = self.ops.release_locks(ino.0, lock_owner.0) {
            tracing::warn!(errno = e, "releasing locks at close failed");
        }
        self.report(ino.0, self.ops.close(ops_handle(fh)), reply);
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // A flock lock goes with the last close of its open file.
        if let Some(owner) = lock_owner {
            if let Err(e) = self.ops.release_locks(ino.0, owner.0) {
                tracing::warn!(errno = e, "releasing locks at close failed");
            }
        }
        // The kernel ignores this reply: a loss stays recorded for later openers.
        match self.ops.release(ops_handle(fh)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.report(ino.0, self.ops.fsync(ops_handle(fh)), reply);
    }

    /// Every change to a directory is committed by the cluster before its call returns, so
    /// there is nothing left to sync.
    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(self.dirs.open(ino.0)), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let stream = self.dirs.get(fh.0, ino.0);
        let mut s = stream.lock().unwrap_or_else(|e| e.into_inner());
        let r = s.read(
            offset,
            |after| self.ops.readdir_page(ino.0, after, crate::dirs::PAGE),
            |i, k, n, off| reply.add(INodeNo(i), off, kind(k), n),
        );
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.dirs.close(fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.ops.statfs() {
            Ok(s) => {
                let c = statfs_counts(&s);
                reply.statfs(
                    c.blocks,
                    c.bfree,
                    c.bfree,
                    c.files,
                    c.ffree,
                    STATFS_BLOCK as u32,
                    atlas_native::namespace::MAX_NAME_BYTES as u32,
                    STATFS_BLOCK as u32,
                )
            }
            Err(e) => reply.error(err(e)),
        }
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        // Permission checks are the kernel's (mounted with default_permissions).
        reply.ok();
    }
}

#[cfg(test)]
mod statfs_tests {
    use atlas_native::FsQuota;

    use super::*;

    fn stat(used_bytes: u64, inodes: u64, quota: Option<FsQuota>) -> FsStat {
        FsStat {
            inodes,
            used_bytes,
            free_list_bytes: 0,
            quota,
        }
    }

    #[test]
    fn statfs_reports_quota_limits_as_capacity() {
        let free = statfs_counts(&stat(10 * 4096, 5, None));
        assert_eq!((free.blocks, free.bfree), (10 + (1 << 28), 1 << 28));
        assert_eq!((free.files, free.ffree), (5 + (1 << 24), 1 << 24));

        let q = Some(FsQuota {
            max_bytes: Some(100 * 4096),
            max_inodes: Some(8),
        });
        let c = statfs_counts(&stat(10 * 4096 + 1, 5, q));
        assert_eq!(
            c,
            StatfsCounts {
                blocks: 100,
                bfree: 89,
                files: 8,
                ffree: 3
            }
        );
        // Over a lowered quota: nothing free, never an underflow.
        let c = statfs_counts(&stat(200 * 4096, 9, q));
        assert_eq!((c.bfree, c.ffree), (0, 0));
    }
}

#[cfg(test)]
mod open_tests {
    use super::*;

    #[test]
    fn o_direct_opens_bypass_the_page_cache() {
        let (fh, f) = handle(7, libc::O_RDWR | libc::O_DIRECT);
        assert_eq!((fh.0 & FH_DIRECT, ops_handle(fh)), (FH_DIRECT, 7));
        assert!(f.contains(FopenFlags::FOPEN_DIRECT_IO | FopenFlags::FOPEN_PARALLEL_DIRECT_WRITES));
        let (fh, f) = handle(7, libc::O_RDWR | libc::O_SYNC);
        assert_eq!((fh.0 & FH_DIRECT, ops_handle(fh)), (0, 7));
        assert_eq!(f, FopenFlags::empty());
        assert!(writable(libc::O_WRONLY) && writable(libc::O_RDWR | libc::O_APPEND));
        assert!(!writable(libc::O_RDONLY | libc::O_DIRECT));
    }
}
