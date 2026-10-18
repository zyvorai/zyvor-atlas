// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Filesystem operations against one native filesystem, independent of the kernel interface:
//! every call returns a value or an errno. Attributes and name lookups are cached for a short
//! TTL, or with cache leases for as long as the cluster's lease on them lasts (a change by
//! another client recalls it first); writes are buffered per inode and sent on fsync, close, a
//! non-sequential write, or once a run reaches the flush size. Full runs are sent in the
//! background, several at once ([`crate::pipeline`]).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use atlas_native::{
    checksum,
    engine::{Attr, DirEntry, FileLayout, FsStat, LayoutExtent, NewNode},
    BlockStore, NodeType, RemoteDevice, SetAttr, TlsIdentity,
};
use reqwest::Method;
use serde_json::json;

use crate::{
    cache::TtlCache,
    client::{encode, Body, Client, Error, Retry},
    locks::{Conflict, Locks, Recall},
    pipeline::{Pipeline, SendRun},
    writeback::{Dirty, WriteBack},
};

pub type Errno = i32;

/// A read-ahead window: its file offset and the bytes read from there.
type Window = (u64, Arc<Vec<u8>>);

#[derive(Debug, Clone)]
pub struct OpsConfig {
    /// How long attributes and name lookups may be served from cache (also the kernel TTL).
    pub ttl: Duration,
    /// Write-back run size; 0 sends every write immediately. A multiple of the filesystem's
    /// extent size lets the cluster write the runs of a sequential stream concurrently.
    pub writeback_bytes: usize,
    /// Full runs sent at once in the background; 0 sends them in the write call.
    pub writeback_parallel: usize,
    /// Largest single read or write request sent to the cluster.
    pub max_io_bytes: usize,
    /// A read fetches at least this much (from the read offset) and serves following reads from
    /// it until the TTL expires or the file is written through this mount; 0 disables it.
    pub readahead_bytes: usize,
    /// Read file data straight from the data nodes instead of through the metadata leader.
    pub direct_reads: Option<DirectReads>,
    /// Lease of the session holding this mount's file locks; renewed every third of it.
    pub session_ttl: Duration,
    /// Blocking lock requests (`F_SETLKW`) allowed to wait at once; keep it below the kernel
    /// worker threads so waiters never block the unlock they wait for.
    pub lock_waiters: usize,
    /// Cache attributes and names under cache leases instead of for `ttl`: nothing cached is
    /// ever stale, and the kernel is given a zero TTL so every lookup reaches this cache.
    pub cache_leases: bool,
}

impl Default for OpsConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(1),
            writeback_bytes: 4 << 20,
            writeback_parallel: 4,
            max_io_bytes: 8 << 20,
            readahead_bytes: 4 << 20,
            direct_reads: None,
            session_ttl: Duration::from_secs(15),
            lock_waiters: 3,
            cache_leases: false,
        }
    }
}

/// Direct reads: the leader only returns where a range's extents live (`/layout`); the client
/// fetches every extent from a replica's data node itself, in parallel, and verifies its
/// SHA-256. Any failure (unreachable node, checksum mismatch, a layout made stale by a
/// concurrent write) falls back to reading through the leader.
#[derive(Debug, Clone)]
pub struct DirectReads {
    /// Client identity for data nodes that require mutual TLS; plaintext otherwise.
    pub identity: Option<Arc<TlsIdentity>>,
    pub timeout: Duration,
    /// Try full copies on this data-node host first (the host this client runs on).
    pub prefer_host: Option<String>,
}

/// Extents a direct read fetches concurrently.
const DIRECT_PARALLELISM: usize = 8;

pub struct Ops {
    client: Arc<Client>,
    /// `<fs>` or `<fs>@<snapshot>` (read-only).
    fs: String,
    pub cfg: OpsConfig,
    attrs: Arc<Mutex<TtlCache<u64, Attr>>>,
    names: Arc<Mutex<TtlCache<(u64, String), u64>>>,
    /// When each inode's cache lease was last recalled: a leased reply to a request sent
    /// before that is not cached.
    recalled: Arc<Mutex<HashMap<u64, Instant>>>,
    dirty: Mutex<WriteBack>,
    /// Sends full runs in the background (with `writeback_parallel` > 0).
    pipeline: Option<Pipeline>,
    readahead: Mutex<TtlCache<u64, Window>>,
    /// Where each inode's last read ended: only a read that continues there reads ahead.
    last_read_end: Mutex<TtlCache<u64, u64>>,
    /// Open handles per inode in this mount.
    opens: Mutex<HashMap<u64, u32>>,
    /// Files unlinked while open here: renamed to a hidden name in their directory and removed
    /// on last close, so open handles keep working (as libfuse does without `hard_remove`).
    hidden: Mutex<HashMap<u64, (u64, String)>>,
    /// Data-node clients for direct reads, by `(node id, endpoint, device)`.
    data_nodes: Mutex<HashMap<(String, String, usize), Arc<RemoteDevice>>>,
    direct_fallbacks: std::sync::atomic::AtomicU64,
    locks: Arc<Locks>,
}

/// Prefix of the names open-but-unlinked files are parked under; hidden from listings.
pub const HIDDEN_PREFIX: &str = ".atlas_hidden_";

fn errno(e: Error) -> Errno {
    tracing::debug!(error = %e, "native fs call failed");
    e.errno()
}

fn decode<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T, Errno> {
    serde_json::from_value(v).map_err(|e| {
        tracing::warn!(error = %e, "undecodable response");
        libc::EIO
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn check_name(name: &str) -> Result<(), Errno> {
    if name.len() > atlas_native::namespace::MAX_NAME_BYTES {
        return Err(libc::ENAMETOOLONG);
    }
    Ok(())
}

impl Ops {
    pub fn new(client: Client, fs: impl Into<String>, cfg: OpsConfig) -> Self {
        let client = Arc::new(client);
        let fs = fs.into();
        // Under cache leases only leased replies are cached.
        let ttl = if cfg.cache_leases {
            Duration::ZERO
        } else {
            cfg.ttl
        };
        let attrs = Arc::new(Mutex::new(TtlCache::new(ttl)));
        let names = Arc::new(Mutex::new(TtlCache::new(ttl)));
        let recalled = Arc::new(Mutex::new(HashMap::new()));
        let recall: Option<Recall> = cfg.cache_leases.then(|| {
            let (attrs, names, recalled) = (attrs.clone(), names.clone(), recalled.clone());
            Arc::new(move |inos: &[u64]| {
                let now = Instant::now();
                if let Ok(mut r) = recalled.lock() {
                    if r.len() >= 65_536 {
                        r.retain(|_, at| now.duration_since(*at) < Duration::from_secs(60));
                    }
                    r.extend(inos.iter().map(|i| (*i, now)));
                }
                if let Ok(mut a) = attrs.lock() {
                    for i in inos {
                        a.remove(i);
                    }
                }
                if let Ok(mut n) = names.lock() {
                    n.retain(|(parent, _), _| !inos.contains(parent));
                }
            }) as Recall
        });
        let locks = Arc::new(Locks::new(
            client.clone(),
            &fs,
            cfg.session_ttl,
            cfg.lock_waiters,
            recall,
        ));
        let pipeline = (cfg.writeback_parallel > 0 && cfg.writeback_bytes > 0).then(|| {
            let send: SendRun = {
                let (client, fs, locks, attrs) =
                    (client.clone(), fs.clone(), locks.clone(), attrs.clone());
                let max_io = cfg.max_io_bytes;
                Arc::new(move |run: &Dirty| {
                    let result = send_run(&client, &fs, &locks, max_io, run).map(drop);
                    // Runs finish out of order, so no reply's attributes are known current.
                    if let Ok(mut c) = attrs.lock() {
                        c.remove(&run.ino);
                    }
                    result
                })
            };
            Pipeline::new(cfg.writeback_parallel, send)
        });
        Self {
            locks,
            pipeline,
            client,
            fs,
            attrs,
            names,
            recalled,
            dirty: Mutex::new(WriteBack::new(cfg.writeback_bytes)),
            readahead: Mutex::new(TtlCache::new(if cfg.readahead_bytes > 0 {
                cfg.ttl
            } else {
                Duration::ZERO
            })),
            last_read_end: Mutex::new(TtlCache::new(Duration::from_secs(10))),
            opens: Mutex::new(HashMap::new()),
            hidden: Mutex::new(HashMap::new()),
            data_nodes: Mutex::new(HashMap::new()),
            direct_fallbacks: std::sync::atomic::AtomicU64::new(0),
            cfg,
        }
    }

    /// Direct reads that fell back to reading through the leader.
    pub fn direct_fallbacks(&self) -> u64 {
        self.direct_fallbacks
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// `F_GETLK`. A snapshot mount has no writers, so nothing ever conflicts there.
    pub fn getlk(
        &self,
        ino: u64,
        owner: u64,
        start: u64,
        end: u64,
        typ: i32,
    ) -> Result<Option<Conflict>, Errno> {
        if self.read_only() {
            return Ok(None);
        }
        self.locks.getlk(ino, owner, start, end, typ)
    }

    /// `F_SETLK`/`F_SETLKW` (`wait`), seen by every mount of the filesystem.
    #[allow(clippy::too_many_arguments)]
    pub fn setlk(
        &self,
        ino: u64,
        owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        wait: bool,
    ) -> Result<(), Errno> {
        if self.read_only() {
            return Ok(());
        }
        self.locks.setlk(ino, owner, start, end, typ, pid, wait)
    }

    /// Drops the owner's locks on `ino`: POSIX locks go on any close by their process.
    pub fn release_locks(&self, ino: u64, owner: u64) -> Result<(), Errno> {
        self.locks.release(ino, owner)
    }

    /// Closes the lock session at unmount, releasing every lock this mount holds.
    pub fn close_session(&self) {
        self.locks.close();
    }

    /// The lock session's id, once this mount took a lock.
    pub fn lock_session(&self) -> Option<String> {
        self.locks.session_id()
    }

    /// Times the lock session expired under this mount (its locks were lost).
    pub fn lock_sessions_lost(&self) -> usize {
        self.locks.sessions_lost()
    }

    /// The mounted filesystem: `<fs>` or `<fs>@<snapshot>`.
    pub fn fs(&self) -> &str {
        &self.fs
    }

    pub fn read_only(&self) -> bool {
        self.fs.contains('@')
    }

    /// The TTL the kernel may cache attributes and names for.
    pub fn kernel_ttl(&self) -> Duration {
        if self.cfg.cache_leases {
            Duration::ZERO
        } else {
            self.cfg.ttl
        }
    }

    fn path(&self, rest: &str) -> String {
        fs_path(&self.fs, &self.locks, rest)
    }

    /// Whether to ask for cache leases: enabled, writable, and the session is open.
    fn leasing(&self) -> bool {
        self.cfg.cache_leases && !self.read_only() && self.locks.session().is_ok()
    }

    /// GET `rest` asking for a cache lease: the attributes, when the request was sent, and
    /// until when the lease lasts (if one was granted).
    fn leased_get(&self, rest: &str) -> Result<(Attr, Instant, Option<Instant>), Errno> {
        let sep = if rest.contains('?') { '&' } else { '?' };
        let sent = Instant::now();
        let v = self.call(
            Method::GET,
            &format!("{rest}{sep}lease=1"),
            Body::Empty,
            Retry::Idempotent,
        )?;
        // Counted from the send, with a margin for clock rate differences.
        let until = v["lease_ms"]
            .as_u64()
            .map(|ms| sent + Duration::from_millis(ms) * 9 / 10);
        Ok((decode(v)?, sent, until))
    }

    /// Whether `ino`'s lease was not recalled since `sent`: a reply to a request sent before a
    /// recall may predate the change the recall was for.
    fn unrecalled(&self, ino: u64, sent: Instant) -> bool {
        self.recalled
            .lock()
            .is_ok_and(|r| r.get(&ino).is_none_or(|at| *at < sent))
    }

    fn call(
        &self,
        method: Method,
        rest: &str,
        body: Body<'_>,
        retry: Retry,
    ) -> Result<serde_json::Value, Errno> {
        self.client
            .json(method, &self.path(rest), body, retry)
            .map_err(errno)
    }

    fn void(&self, method: Method, rest: &str, body: Body<'_>, retry: Retry) -> Result<(), Errno> {
        self.client
            .request(method, &self.path(rest), body, retry)
            .map(drop)
            .map_err(errno)
    }

    /// Caches `a` and returns it with any buffered bytes counted in its size.
    fn remember(&self, a: Attr) -> Attr {
        if let Ok(mut c) = self.attrs.lock() {
            c.put(a.ino, a.clone());
        }
        self.adjust(a)
    }

    /// `a` with this mount's buffered bytes and parked unlinks applied.
    fn adjust(&self, mut a: Attr) -> Attr {
        if let Some(end) = self.dirty.lock().ok().and_then(|d| d.end(a.ino)) {
            a.size = a.size.max(end);
        }
        if let Some(end) = self.pipeline.as_ref().and_then(|p| p.end(a.ino)) {
            a.size = a.size.max(end);
        }
        if self.hidden.lock().is_ok_and(|h| h.contains_key(&a.ino)) {
            a.nlink = a.nlink.saturating_sub(1);
        }
        a
    }

    pub fn opened(&self, ino: u64) {
        if let Ok(mut o) = self.opens.lock() {
            *o.entry(ino).or_default() += 1;
        }
    }

    /// Drops one open handle; the last one sends buffered writes and removes the file if it
    /// was unlinked while open.
    pub fn released(&self, ino: u64) -> Result<(), Errno> {
        let flushed = self.flush(ino);
        let last = self.opens.lock().is_ok_and(|mut o| match o.get_mut(&ino) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            _ => {
                o.remove(&ino);
                true
            }
        });
        if last {
            let parked = self.hidden.lock().ok().and_then(|mut h| h.remove(&ino));
            if let Some((parent, name)) = parked {
                self.forget_attr(ino);
                self.remove_entry(parent, &name)?;
            }
        }
        flushed
    }

    fn remove_entry(&self, parent: u64, name: &str) -> Result<(), Errno> {
        self.void(
            Method::POST,
            &format!("/inodes/{parent}/unlink"),
            Body::Json(json!({ "name": name })),
            Retry::Remove,
        )
    }

    fn forget_attr(&self, ino: u64) {
        if let Ok(mut c) = self.attrs.lock() {
            c.remove(&ino);
        }
    }

    pub fn getattr(&self, ino: u64) -> Result<Attr, Errno> {
        if let Some(a) = self.attrs.lock().ok().and_then(|mut c| c.get(&ino)) {
            return Ok(self.adjust(a));
        }
        if self.leasing() {
            let (a, sent, until) = self.leased_get(&format!("/inodes/{ino}"))?;
            if let Some(until) = until.filter(|_| self.unrecalled(ino, sent)) {
                if let Ok(mut c) = self.attrs.lock() {
                    c.put_until(ino, a.clone(), until);
                }
            }
            return Ok(self.adjust(a));
        }
        let a = decode(self.call(
            Method::GET,
            &format!("/inodes/{ino}"),
            Body::Empty,
            Retry::Idempotent,
        )?)?;
        Ok(self.remember(a))
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Result<Attr, Errno> {
        check_name(name)?;
        let key = (parent, name.to_string());
        if let Some(ino) = self.names.lock().ok().and_then(|mut n| n.get(&key)) {
            if let Ok(a) = self.getattr(ino) {
                return Ok(a);
            }
        }
        let rest = format!("/inodes/{parent}/lookup?name={}", encode(name));
        if self.leasing() {
            // One lease covers the name (the directory's), one the child's attributes.
            let (a, sent, until) = self.leased_get(&rest)?;
            if let Some(until) = until {
                if self.unrecalled(parent, sent) {
                    if let Ok(mut n) = self.names.lock() {
                        n.put_until(key, a.ino, until);
                    }
                }
                if self.unrecalled(a.ino, sent) {
                    if let Ok(mut c) = self.attrs.lock() {
                        c.put_until(a.ino, a.clone(), until);
                    }
                }
            }
            return Ok(self.adjust(a));
        }
        let a: Attr = decode(self.call(Method::GET, &rest, Body::Empty, Retry::Idempotent)?)?;
        if let Ok(mut n) = self.names.lock() {
            n.put(key, a.ino);
        }
        Ok(self.remember(a))
    }

    /// `.` (the directory) or `..` (its parent) of directory `dir`, which the kernel looks up
    /// to turn a file handle back into an inode (NFS re-export). Not cached: a rename moves `..`.
    pub fn lookup_dot(&self, dir: u64, name: &str) -> Result<Attr, Errno> {
        let rest = format!("/inodes/{dir}/lookup?name={}", encode(name));
        let a: Attr = decode(self.call(Method::GET, &rest, Body::Empty, Retry::Idempotent)?)?;
        self.getattr(a.ino)
    }

    /// Every visible entry of a directory.
    pub fn readdir(&self, ino: u64) -> Result<Vec<DirEntry>, Errno> {
        let mut all = Vec::new();
        let mut after = None;
        loop {
            let (entries, next) = self.readdir_page(ino, after.as_deref(), crate::dirs::PAGE)?;
            all.extend(entries);
            match next {
                Some(n) => after = Some(n),
                None => return Ok(all),
            }
        }
    }

    /// Up to `limit` entries named after `after`: the visible ones, and the name to continue
    /// after (`None` once the directory is done).
    pub fn readdir_page(
        &self,
        ino: u64,
        after: Option<&str>,
        limit: usize,
    ) -> Result<crate::dirs::Page, Errno> {
        let mut rest = format!("/inodes/{ino}/entries?limit={limit}");
        if let Some(a) = after {
            rest.push_str(&format!("&after={}", encode(a)));
        }
        let v = self.call(Method::GET, &rest, Body::Empty, Retry::Idempotent)?;
        let mut entries: Vec<DirEntry> = decode(v["entries"].clone())?;
        let next = match entries.last() {
            Some(e) if entries.len() >= limit => Some(e.name.clone()),
            _ => None,
        };
        entries.retain(|e| !e.name.starts_with(HIDDEN_PREFIX));
        Ok((entries, next))
    }

    pub fn readlink(&self, ino: u64) -> Result<String, Errno> {
        let v = self.call(
            Method::GET,
            &format!("/inodes/{ino}/target"),
            Body::Empty,
            Retry::Idempotent,
        )?;
        v["target"].as_str().map(str::to_string).ok_or(libc::EIO)
    }

    pub fn listxattr(&self, ino: u64) -> Result<Vec<String>, Errno> {
        let v = self.call(
            Method::GET,
            &format!("/inodes/{ino}/xattrs"),
            Body::Empty,
            Retry::Idempotent,
        )?;
        serde_json::from_value(v["names"].clone()).map_err(|_| libc::EIO)
    }

    pub fn getxattr(&self, ino: u64, name: &str) -> Result<Vec<u8>, Errno> {
        self.client
            .request(
                Method::GET,
                &self.path(&format!("/inodes/{ino}/xattrs/{}", encode(name))),
                Body::Empty,
                Retry::Idempotent,
            )
            .map_err(errno)
    }

    /// `create`/`replace` follow `XATTR_CREATE`/`XATTR_REPLACE`.
    pub fn setxattr(
        &self,
        ino: u64,
        name: &str,
        value: &[u8],
        create: bool,
        replace: bool,
    ) -> Result<(), Errno> {
        let mode = match (create, replace) {
            (true, true) => return Err(libc::EINVAL),
            (true, false) => "create",
            (false, true) => "replace",
            (false, false) => "set",
        };
        self.forget_attr(ino);
        self.void(
            Method::PUT,
            &format!("/inodes/{ino}/xattrs/{}?mode={mode}", encode(name)),
            Body::Bytes(value),
            Retry::Idempotent,
        )
    }

    pub fn removexattr(&self, ino: u64, name: &str) -> Result<(), Errno> {
        self.forget_attr(ino);
        self.void(
            Method::DELETE,
            &format!("/inodes/{ino}/xattrs/{}", encode(name)),
            Body::Empty,
            Retry::Remove,
        )
    }

    pub fn statfs(&self) -> Result<FsStat, Errno> {
        decode(self.call(Method::GET, "/statfs", Body::Empty, Retry::Idempotent)?)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mknode(
        &self,
        parent: u64,
        name: &str,
        kind: NodeType,
        target: Option<String>,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Attr, Errno> {
        self.create(
            parent,
            NewNode {
                name: name.into(),
                op_id: String::new(),
                kind,
                target,
                rdev: 0,
                mode,
                create_mode: None,
                uid,
                gid,
            },
        )
    }

    /// Creates `node` in `parent` under a fresh op id: a retry of this request is recognised,
    /// a later create of the same name is EEXIST.
    pub fn create(&self, parent: u64, mut node: NewNode) -> Result<Attr, Errno> {
        check_name(&node.name)?;
        node.op_id = uuid::Uuid::new_v4().to_string();
        let name = node.name.clone();
        let body = serde_json::to_value(&node).map_err(|_| libc::EINVAL)?;
        let a: Attr = decode(self.call(
            Method::POST,
            &format!("/inodes/{parent}/entries"),
            Body::Json(body),
            Retry::Idempotent,
        )?)?;
        self.forget_attr(parent);
        if let Ok(mut n) = self.names.lock() {
            n.put((parent, name), a.ino);
        }
        Ok(self.remember(a))
    }

    pub fn link(&self, ino: u64, parent: u64, name: &str) -> Result<Attr, Errno> {
        check_name(name)?;
        let a: Attr = decode(self.call(
            Method::POST,
            &format!("/inodes/{ino}/links"),
            Body::Json(json!({ "parent": parent, "name": name })),
            // Linking the same inode under the same name again is a no-op on the server.
            Retry::Idempotent,
        )?)?;
        self.forget_attr(parent);
        Ok(self.remember(a))
    }

    fn drop_name(&self, parent: u64, name: &str) {
        let ino = self
            .names
            .lock()
            .ok()
            .and_then(|mut n| n.remove(&(parent, name.to_string())));
        self.forget_attr(parent);
        if let Some(ino) = ino {
            self.forget_attr(ino);
        }
    }

    pub fn unlink(&self, parent: u64, name: &str) -> Result<(), Errno> {
        if let Ok(a) = self.lookup(parent, name) {
            let open = self.opens.lock().is_ok_and(|o| o.contains_key(&a.ino));
            if open && a.nlink == 1 && a.kind != NodeType::Dir {
                let parked = format!(
                    "{HIDDEN_PREFIX}{}_{}",
                    a.ino,
                    &uuid::Uuid::new_v4().simple().to_string()[..8]
                );
                self.rename(parent, name, parent, &parked)?;
                if let Ok(mut h) = self.hidden.lock() {
                    h.insert(a.ino, (parent, parked));
                }
                return Ok(());
            }
            // Buffered bytes must not land on an inode that is about to disappear.
            self.flush(a.ino)?;
        }
        self.drop_name(parent, name);
        self.remove_entry(parent, name)
    }

    pub fn rmdir(&self, parent: u64, name: &str) -> Result<(), Errno> {
        self.drop_name(parent, name);
        self.void(
            Method::POST,
            &format!("/inodes/{parent}/rmdir"),
            Body::Json(json!({ "name": name })),
            Retry::Remove,
        )
    }

    pub fn rename(
        &self,
        parent: u64,
        name: &str,
        new_parent: u64,
        new_name: &str,
    ) -> Result<(), Errno> {
        check_name(new_name)?;
        self.drop_name(parent, name);
        self.drop_name(new_parent, new_name);
        // Both inodes' ctime and link counts may change; re-fetch every attribute.
        if let Ok(mut c) = self.attrs.lock() {
            c.clear();
        }
        self.void(
            Method::POST,
            "/rename",
            Body::Json(json!({
                "parent": parent,
                "name": name,
                "new_parent": new_parent,
                "new_name": new_name,
            })),
            Retry::Remove,
        )
    }

    pub fn setattr(&self, ino: u64, attr: SetAttr) -> Result<Attr, Errno> {
        self.drop_readahead(ino);
        if let Some(size) = attr.size {
            // Bytes buffered below the new size must reach the file before it is cut.
            self.flush(ino)?;
            if let Ok(mut d) = self.dirty.lock() {
                d.truncate(ino, size);
            }
        }
        let body = serde_json::to_value(&attr).map_err(|_| libc::EINVAL)?;
        let a = decode(self.call(
            Method::POST,
            &format!("/inodes/{ino}/attr"),
            Body::Json(body),
            Retry::Idempotent,
        )?)?;
        Ok(self.remember(a))
    }

    pub fn read(&self, ino: u64, offset: u64, len: usize) -> Result<Vec<u8>, Errno> {
        self.flush(ino)?;
        let cached = self.readahead.lock().ok().and_then(|mut r| r.get(&ino));
        if let Some((at, buf)) = cached {
            let end = at + buf.len() as u64;
            // A window shorter than requested ended at EOF, so it still answers fully.
            if offset >= at && (offset + len as u64 <= end || buf.len() < self.cfg.readahead_bytes)
            {
                let from = ((offset - at) as usize).min(buf.len());
                let to = (from + len).min(buf.len());
                if let Ok(mut l) = self.last_read_end.lock() {
                    l.put(ino, at + to as u64);
                }
                return Ok(buf[from..to].to_vec());
            }
        }
        let sequential = self
            .last_read_end
            .lock()
            .ok()
            .and_then(|mut l| {
                let prev = l.get(&ino);
                l.put(ino, offset + len as u64);
                prev
            })
            .map_or(offset == 0, |end| end == offset);
        let want = if sequential {
            len.max(self.cfg.readahead_bytes)
        } else {
            len
        };
        let data = self.fetch(ino, offset, want)?;
        let out = data[..len.min(data.len())].to_vec();
        if want > len {
            if let Ok(mut r) = self.readahead.lock() {
                r.put(ino, (offset, Arc::new(data)));
            }
        }
        Ok(out)
    }

    fn fetch(&self, ino: u64, offset: u64, len: usize) -> Result<Vec<u8>, Errno> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let want = (len - out.len()).min(self.cfg.max_io_bytes);
            let at = offset + out.len() as u64;
            let direct = match &self.cfg.direct_reads {
                Some(d) => match self.fetch_direct(d, ino, at, want) {
                    Ok(chunk) => Some(chunk),
                    Err(e) => {
                        tracing::debug!(ino, offset = at, error = %e, "direct read fell back");
                        self.direct_fallbacks
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        None
                    }
                },
                None => None,
            };
            let chunk = match direct {
                Some(chunk) => chunk,
                None => self
                    .client
                    .request(
                        Method::GET,
                        &self.path(&format!("/inodes/{ino}/data?offset={at}&len={want}")),
                        Body::Empty,
                        Retry::Idempotent,
                    )
                    .map_err(errno)?,
            };
            let short = chunk.len() < want;
            out.extend_from_slice(&chunk);
            if short {
                break;
            }
        }
        Ok(out)
    }

    /// Up to `len` bytes at `offset`, fetched from the data nodes; see [`DirectReads`].
    fn fetch_direct(
        &self,
        d: &DirectReads,
        ino: u64,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, String> {
        let layout: FileLayout = self
            .call(
                Method::GET,
                &format!("/inodes/{ino}/layout?offset={offset}&len={len}"),
                Body::Empty,
                Retry::Idempotent,
            )
            .and_then(decode)
            .map_err(|e| format!("layout: errno {e}"))?;
        let mut out = vec![0u8; layout.len];
        let end = offset + layout.len as u64;
        for window in layout.extents.chunks(DIRECT_PARALLELISM) {
            let bufs: Vec<Result<Vec<u8>, String>> = std::thread::scope(|s| {
                let handles: Vec<_> = window
                    .iter()
                    .map(|ext| s.spawn(|| self.read_extent_direct(d, ext)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or_else(|_| Err("reader panicked".into())))
                    .collect()
            });
            for (ext, buf) in window.iter().zip(bufs) {
                let buf = buf?;
                let start = ext.logical_offset.max(offset);
                let stop = (ext.logical_offset + ext.len as u64).min(end);
                if start >= stop {
                    continue;
                }
                out[(start - offset) as usize..(stop - offset) as usize].copy_from_slice(
                    &buf[(start - ext.logical_offset) as usize
                        ..(stop - ext.logical_offset) as usize],
                );
            }
        }
        Ok(out)
    }

    /// One whole extent from the first replica whose checksum verifies, or from the data shards
    /// of an erasure-coded one (a missing or bad shard falls back to the leader, which rebuilds).
    fn read_extent_direct(&self, d: &DirectReads, ext: &LayoutExtent) -> Result<Vec<u8>, String> {
        let payload = self.read_payload_direct(d, ext)?;
        if ext.payload_len > 0 {
            return atlas_native::compress::decompress(&payload, ext.len).map_err(|e| e.to_string());
        }
        Ok(payload)
    }

    /// The bytes the devices hold for `ext`: its data, or its LZ4 payload when compressed.
    fn read_payload_direct(&self, d: &DirectReads, ext: &LayoutExtent) -> Result<Vec<u8>, String> {
        let stored = if ext.payload_len > 0 { ext.payload_len } else { ext.len };
        if let Some(ec) = &ext.ec {
            let mut out = Vec::with_capacity(ec.data * ec.shard_len);
            for (i, r) in ext.replicas.iter().take(ec.data).enumerate() {
                let endpoint = r
                    .endpoint
                    .as_ref()
                    .ok_or_else(|| format!("shard {i} on {} has no endpoint", r.node_id))?;
                let dev = self.data_node(d, &r.node_id, endpoint, r.device_index)?;
                let shard = dev
                    .read_exact_at(r.offset, ec.shard_len)
                    .map_err(|e| format!("shard {i} on {}: {e}", r.node_id))?;
                if ec.shard_checksums.get(i) != Some(&hex(&checksum::sha256(&shard))) {
                    return Err(format!("shard {i} checksum mismatch on {}", r.node_id));
                }
                out.extend_from_slice(&shard);
            }
            out.truncate(stored);
            if hex(&checksum::sha256(&out)) != ext.checksum {
                return Err("erasure-coded extent checksum mismatch".into());
            }
            return Ok(out);
        }
        let mut last = format!("extent at {} has no reachable replica", ext.logical_offset);
        let mut order: Vec<_> = ext.replicas.iter().collect();
        if let Some(h) = &d.prefer_host {
            order.sort_by_key(|r| r.host.as_ref() != Some(h));
        }
        for r in order {
            let Some(endpoint) = &r.endpoint else {
                continue;
            };
            let dev = self.data_node(d, &r.node_id, endpoint, r.device_index)?;
            match dev.read_exact_at(r.offset, stored) {
                Ok(buf) if hex(&checksum::sha256(&buf)) == ext.checksum => return Ok(buf),
                Ok(_) => last = format!("checksum mismatch on {}", r.node_id),
                Err(e) => last = format!("{}: {e}", r.node_id),
            }
        }
        Err(last)
    }

    fn data_node(
        &self,
        d: &DirectReads,
        node_id: &str,
        endpoint: &str,
        device: usize,
    ) -> Result<Arc<RemoteDevice>, String> {
        let mut nodes = self
            .data_nodes
            .lock()
            .map_err(|_| "data-node cache poisoned")?;
        let key = (node_id.to_string(), endpoint.to_string(), device);
        if let Some(dev) = nodes.get(&key) {
            return Ok(dev.clone());
        }
        let dev = Arc::new(
            match &d.identity {
                Some(id) => RemoteDevice::with_tls(endpoint, node_id, id, d.timeout)
                    .map_err(|e| format!("{node_id}: {e}"))?,
                None => RemoteDevice::new(endpoint, d.timeout),
            }
            .on_device(device),
        );
        nodes.insert(key, dev.clone());
        Ok(dev)
    }

    fn drop_readahead(&self, ino: u64) {
        if let Ok(mut r) = self.readahead.lock() {
            r.remove(&ino);
        }
    }

    pub fn write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<usize, Errno> {
        if self.read_only() {
            return Err(libc::EROFS);
        }
        self.drop_readahead(ino);
        if let Some(e) = self.pipeline.as_ref().and_then(|p| p.failed(ino)) {
            return Err(e);
        }
        // Runs are queued under the lock, so runs of one inode queue in write order.
        let mut d = self.dirty.lock().map_err(|_| libc::EIO)?;
        for run in d.write(ino, offset, data) {
            self.dispatch(run)?;
        }
        Ok(data.len())
    }

    /// An `O_DIRECT` write: whatever this mount buffered for the file goes first, then `data` is
    /// committed on its replicas before this returns, bypassing write-back.
    pub fn write_through(&self, ino: u64, offset: u64, data: &[u8]) -> Result<usize, Errno> {
        if self.read_only() {
            return Err(libc::EROFS);
        }
        self.drop_readahead(ino);
        self.flush(ino)?;
        self.send(&Dirty {
            ino,
            offset,
            data: data.to_vec(),
        })?;
        Ok(data.len())
    }

    /// An `O_DIRECT` read: from the cluster, around the read-ahead window (after this mount's
    /// buffered writes to the file are sent).
    pub fn read_direct(&self, ino: u64, offset: u64, len: usize) -> Result<Vec<u8>, Errno> {
        self.flush(ino)?;
        self.fetch(ino, offset, len)
    }

    fn dispatch(&self, run: Dirty) -> Result<(), Errno> {
        match &self.pipeline {
            Some(p) => p.submit(run),
            None => self.send(&run),
        }
    }

    /// Sends any buffered bytes of `ino` and waits for its runs in flight.
    pub fn flush(&self, ino: u64) -> Result<(), Errno> {
        {
            let mut d = self.dirty.lock().map_err(|_| libc::EIO)?;
            if let Some(run) = d.take(ino) {
                self.dispatch(run)?;
            }
        }
        match &self.pipeline {
            Some(p) => p.wait(ino),
            None => Ok(()),
        }
    }

    /// Sends every buffered write and removes files parked by unlink-while-open (at unmount).
    pub fn flush_all(&self) -> Result<(), Errno> {
        let mut result = Ok(());
        {
            let mut d = self.dirty.lock().map_err(|_| libc::EIO)?;
            for run in d.take_all() {
                if let Err(e) = self.dispatch(run) {
                    result = Err(e);
                }
            }
        }
        if let Some(Err(e)) = self.pipeline.as_ref().map(Pipeline::wait_all) {
            result = Err(e);
        }
        let parked: Vec<(u64, String)> = self
            .hidden
            .lock()
            .map(|mut h| h.drain().map(|(_, v)| v).collect())
            .unwrap_or_default();
        for (parent, name) in parked {
            if let Err(e) = self.remove_entry(parent, &name) {
                result = Err(e);
            }
        }
        result
    }

    /// Writes a run, then caches the attributes the cluster returned.
    fn send(&self, run: &Dirty) -> Result<(), Errno> {
        if let Some(v) = send_run(
            &self.client,
            &self.fs,
            &self.locks,
            self.cfg.max_io_bytes,
            run,
        )? {
            let a: Attr = decode(v)?;
            if let Ok(mut c) = self.attrs.lock() {
                c.put(a.ino, a);
            }
        }
        Ok(())
    }
}

/// Requests name the mount's session (once it has one), so the cluster does not recall the
/// mount's own cache leases for its own changes.
fn fs_path(fs: &str, locks: &Locks, rest: &str) -> String {
    match locks.session_id() {
        Some(s) => {
            let sep = if rest.contains('?') { '&' } else { '?' };
            format!("/v1/fs/{fs}{rest}{sep}session={s}")
        }
        None => format!("/v1/fs/{fs}{rest}"),
    }
}

/// Writes a run in requests of at most `max_io` bytes (each is idempotent: same bytes, same
/// offset); returns the last reply (the file's attributes).
fn send_run(
    client: &Client,
    fs: &str,
    locks: &Locks,
    max_io: usize,
    run: &Dirty,
) -> Result<Option<serde_json::Value>, Errno> {
    let mut last = None;
    for (i, chunk) in run.data.chunks(max_io.max(1)).enumerate() {
        let at = run.offset + (i * max_io.max(1)) as u64;
        let path = fs_path(fs, locks, &format!("/inodes/{}/data?offset={at}", run.ino));
        let v = client
            .json(Method::PUT, &path, Body::Bytes(chunk), Retry::Idempotent)
            .map_err(errno)?;
        last = Some(v);
    }
    Ok(last)
}
