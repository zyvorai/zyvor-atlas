// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! "What changed since the clone": the paths a filesystem created, modified, deleted and renamed
//! relative to the snapshot it was cloned from ([`NativeEngine::fs_changes`]). Read-only, metadata
//! only: file data, symlink targets and extended attribute names or values are never returned.
//!
//! A clone keeps its snapshot's inode numbers and numbers are never reused, so an inode number in
//! both trees is the same file object. That is what makes a rename recognisable: one object that
//! lost exactly one name and gained exactly one. With several names lost or gained (hard links) the
//! pairing is not knowable from the two trees, so those are reported as deleted and created.
//!
//! Cost: one pass over both inode tables and both sets of directory entries, linear in their size.
//! The catalog lock is held only to take a consistent view of the live tree and to copy references
//! to the snapshot's inodes in batches, never across the comparison. The result is sorted, capped
//! ([`ChangeLimits`]) and kept for a few minutes so that later pages cost only their own size and
//! describe the same instant as the first.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, PoisonError,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use super::{NativeEngine, NativeError};
use crate::{
    inodes::InodeTable,
    metadata::MetaError,
    namespace::{node_type, Inode, InodeKind, NodeType, ROOT_INO},
};

/// Changes one page returns unless the request says otherwise.
pub const DEFAULT_CHANGES_LIMIT: usize = 1000;
/// Most changes one page returns.
pub const MAX_CHANGES_LIMIT: usize = 10_000;

/// Inodes or entries read per step (and per catalog lock acquisition for the snapshot).
const BATCH: usize = 1024;
/// Listings computed at once; each holds memory proportional to the two trees while it runs.
const MAX_CONCURRENT: usize = 2;
/// Finished listings kept for later pages: how many, how many per filesystem, how many changes
/// between them, and for how long.
const CACHED_LISTINGS: usize = 8;
const CACHED_PER_FS: usize = 2;
const CACHED_CHANGES: usize = 200_000;
const CACHE_TTL: Duration = Duration::from_secs(300);

/// What one listing may cost. A listing over any limit is refused, never cut short.
#[derive(Debug, Clone, Copy)]
pub struct ChangeLimits {
    /// Inodes in either tree.
    pub max_inodes: u64,
    /// Directory entries in either tree.
    pub max_entries: u64,
    /// Changes in the listing.
    pub max_changes: usize,
    /// Bytes of all paths in the listing.
    pub max_path_bytes: usize,
    /// Time to compute it. Below the gateway driver's default request timeout (10 s).
    pub deadline: Duration,
}

impl Default for ChangeLimits {
    fn default() -> Self {
        Self {
            max_inodes: 1_000_000,
            max_entries: 2_000_000,
            max_changes: 100_000,
            max_path_bytes: 16 << 20,
            deadline: Duration::from_secs(8),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// A name the snapshot did not have.
    Created,
    /// A name both have, whose object or metadata differs.
    Modified,
    /// A name only the snapshot has.
    Deleted,
    /// One object that moved from one name to another (`from` to `path`).
    Renamed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangedField {
    /// File data was written or cut (the bytes may still be equal), or a symlink retargeted.
    Content,
    Size,
    Mode,
    Owner,
    Xattrs,
    /// The hard link count.
    Links,
    Mtime,
    Atime,
    Ctime,
}

/// One changed path. `path` is a path in the workspace, except for [`ChangeKind::Deleted`], where
/// it is a path in the snapshot; `from` is always a path in the snapshot. A renamed directory
/// stands for everything under it that is not listed itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsChange {
    pub path: String,
    pub change: ChangeKind,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// How the object differs from the same object in the snapshot. Empty for a new object, a
    /// deletion and a replacement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<ChangedField>,
    /// The path existed in the snapshot but held a different object.
    #[serde(default)]
    pub replaced: bool,
    /// With `replaced`: the old object's type, if it differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_type: Option<NodeType>,
    /// The object exists in both trees (a created hard link, a deleted one of several names, a
    /// rename or an edit).
    #[serde(default)]
    pub object_in_both: bool,
    /// Inode number: entries with the same one are names of the same object.
    pub ino: u64,
    /// File size in the workspace (in the snapshot for a deletion).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSummary {
    pub created: u64,
    pub modified: u64,
    pub deleted: u64,
    pub renamed: u64,
}

/// One page of a listing, in path order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsChangesPage {
    pub fs: String,
    pub base_snapshot: String,
    /// The metadata log index the listing describes.
    pub as_of_index: u64,
    /// When it was computed, Unix milliseconds.
    pub computed_unix_ms: u64,
    /// Changes in the whole listing.
    pub total: u64,
    pub summary: ChangeSummary,
    pub changes: Vec<FsChange>,
    /// Pass as `cursor` for the next page; `None` on the last.
    #[serde(default)]
    pub next: Option<String>,
}

/// The listing cannot be given as things stand (HTTP 409).
fn refused(m: impl Into<String>) -> NativeError {
    MetaError::Unsupported(m.into()).into()
}

/// The listing would cost more than [`ChangeLimits`] allow (HTTP 409).
fn too_big(m: impl Into<String>) -> NativeError {
    MetaError::TooBig(m.into()).into()
}

fn size_of(i: &Inode) -> Option<u64> {
    match &i.kind {
        InodeKind::File { size, .. } => Some(*size),
        _ => None,
    }
}

/// How two versions of one object differ. A directory's size, link count and times follow its
/// entries, which are reported themselves, so only its own attributes count.
fn compare(old: &Inode, new: &Inode) -> Vec<ChangedField> {
    use ChangedField::*;
    let mut out = Vec::new();
    match (&old.kind, &new.kind) {
        (
            InodeKind::File {
                size: sa,
                extents: ea,
                deltas: da,
            },
            InodeKind::File {
                size: sb,
                extents: eb,
                deltas: db,
            },
        ) => {
            // A snapshot has no pending deltas, so any in the live file were written since.
            if sa != sb || ea != eb || da != db {
                out.push(Content);
            }
            if sa != sb {
                out.push(Size);
            }
        }
        (InodeKind::Symlink { target: a }, InodeKind::Symlink { target: b }) if a != b => {
            out.push(Content)
        }
        (InodeKind::Special { rdev: a, .. }, InodeKind::Special { rdev: b, .. }) if a != b => {
            out.push(Content)
        }
        _ => {}
    }
    if old.mode != new.mode {
        out.push(Mode);
    }
    if (old.uid, old.gid) != (new.uid, new.gid) {
        out.push(Owner);
    }
    if old.xattrs != new.xattrs {
        out.push(Xattrs);
    }
    if !new.is_dir() {
        if old.nlink != new.nlink {
            out.push(Links);
        }
        if old.mtime_ns != new.mtime_ns {
            out.push(Mtime);
        }
        if old.atime_ns != new.atime_ns {
            out.push(Atime);
        }
        if old.ctime_ns != new.ctime_ns {
            out.push(Ctime);
        }
    }
    out
}

/// What the comparison keeps of one inode number.
#[derive(Default)]
struct Seen {
    old: Option<(NodeType, Option<u64>)>,
    new: Option<(NodeType, Option<u64>)>,
    changed: Vec<ChangedField>,
}

/// A name: directory inode and entry name.
type Name = (u64, String);

struct Budget {
    until: Instant,
    limits: ChangeLimits,
}

impl Budget {
    fn check_time(&self) -> Result<(), NativeError> {
        if Instant::now() > self.until {
            return Err(too_big(format!(
                "listing the changes took more than {} ms; refused",
                self.limits.deadline.as_millis()
            )));
        }
        Ok(())
    }

    fn too_many(&self) -> NativeError {
        too_big(format!(
            "more than {} changes; the listing is refused rather than cut short",
            self.limits.max_changes
        ))
    }
}

/// A directory's entries in name order, read a batch at a time.
struct Entries<'a> {
    table: Option<(&'a InodeTable, &'a Inode)>,
    memory: Option<Box<dyn Iterator<Item = (String, u64)> + 'a>>,
    batch: std::vec::IntoIter<(String, u64)>,
    after: Option<String>,
}

impl<'a> Entries<'a> {
    /// The entries of a snapshot directory, which are all in its inode.
    fn frozen(dir: Option<&'a Inode>) -> Self {
        let memory = dir.and_then(|d| match &d.kind {
            InodeKind::Dir { entries, .. } => {
                Some(Box::new(entries.in_memory().map(|(k, v)| (k.clone(), *v)))
                    as Box<dyn Iterator<Item = (String, u64)> + 'a>)
            }
            _ => None,
        });
        Self {
            table: None,
            memory,
            batch: Vec::new().into_iter(),
            after: None,
        }
    }

    /// The entries of a directory of the live table, which may page them from the store.
    fn live(table: &'a InodeTable, dir: Option<&'a Inode>) -> Self {
        Self {
            table: dir.map(|d| (table, d)),
            memory: None,
            batch: Vec::new().into_iter(),
            after: None,
        }
    }

    fn next(&mut self, budget: &Budget) -> Result<Option<(String, u64)>, NativeError> {
        if let Some(m) = &mut self.memory {
            return Ok(m.next());
        }
        if let Some(e) = self.batch.next() {
            return Ok(Some(e));
        }
        let Some((table, dir)) = self.table else {
            return Ok(None);
        };
        budget.check_time()?;
        let batch = table.entries(dir, self.after.as_deref(), BATCH)?;
        if batch.len() < BATCH {
            self.table = None;
        }
        self.after = batch.last().map(|(n, _)| n.clone());
        self.batch = batch.into_iter();
        Ok(self.batch.next())
    }
}

/// Paths of one tree's directories, from each directory's name in its parent.
struct Paths {
    parents: HashMap<u64, Name>,
    memo: HashMap<u64, Arc<str>>,
}

impl Paths {
    fn new(parents: HashMap<u64, Name>) -> Self {
        Self {
            parents,
            memo: HashMap::new(),
        }
    }

    /// The path of directory `ino`; the root's is empty.
    fn dir(&mut self, ino: u64) -> Result<Arc<str>, NativeError> {
        let mut names = Vec::new();
        let mut cur = ino;
        let mut prefix: Arc<str> = "".into();
        while cur != ROOT_INO {
            if let Some(p) = self.memo.get(&cur) {
                prefix = p.clone();
                break;
            }
            let (parent, name) = self.parents.get(&cur).ok_or_else(|| {
                MetaError::Store(format!("directory {cur} is not reachable from the root"))
            })?;
            names.push(name.as_str());
            if names.len() > self.parents.len() {
                return Err(MetaError::Store(format!("directory {ino} is in a loop")).into());
            }
            cur = *parent;
        }
        let mut path = prefix.to_string();
        for n in names.iter().rev() {
            path.push('/');
            path.push_str(n);
        }
        let path: Arc<str> = path.into();
        if ino != ROOT_INO {
            self.memo.insert(ino, path.clone());
        }
        Ok(path)
    }

    fn name(&mut self, (dir, name): &Name) -> Result<String, NativeError> {
        Ok(format!("{}/{name}", self.dir(*dir)?))
    }
}

/// Every change from the snapshot tree `base` (all its inodes, in inode order, entries in memory)
/// to the live table `live`, sorted by path.
pub(crate) fn compute(
    base: &[Arc<Inode>],
    live: &InodeTable,
    limits: ChangeLimits,
) -> Result<Vec<FsChange>, NativeError> {
    let budget = Budget {
        until: Instant::now() + limits.deadline,
        limits,
    };

    // Pass 1: both inode tables, merged in inode order.
    let mut seen: HashMap<u64, Seen> = HashMap::new();
    let mut dirs: BTreeMap<u64, (Option<&Inode>, Option<Arc<Inode>>)> = BTreeMap::new();
    let mut b = 0;
    let mut from = 0;
    let mut count = 0u64;
    loop {
        budget.check_time()?;
        let page = live.page(from, BATCH)?;
        let end = match page.last() {
            Some(last) if page.len() == BATCH => last.ino,
            _ => u64::MAX,
        };
        count += page.len() as u64;
        if count > limits.max_inodes {
            return Err(too_big(format!(
                "the filesystem has more than {} inodes; refused",
                limits.max_inodes
            )));
        }
        let mut l = 0;
        loop {
            let old = base.get(b).filter(|o| o.ino <= end);
            let new = page.get(l);
            let (old, new) = match (old, new) {
                (None, None) => break,
                (Some(o), Some(n)) if o.ino == n.ino => (Some(o), Some(n)),
                (Some(o), Some(n)) if o.ino < n.ino => (Some(o), None),
                (Some(o), None) => (Some(o), None),
                (_, Some(n)) => (None, Some(n)),
            };
            b += usize::from(old.is_some());
            l += usize::from(new.is_some());
            let ino = old.or(new).map_or(0, |i| i.ino);
            let changed = match (old, new) {
                (Some(o), Some(n)) if !Arc::ptr_eq(o, n) => compare(o, n),
                _ => Vec::new(),
            };
            if old.is_some_and(|i| i.is_dir()) || new.is_some_and(|i| i.is_dir()) {
                dirs.insert(
                    ino,
                    (
                        old.filter(|i| i.is_dir()).map(|i| &**i),
                        new.filter(|i| i.is_dir()).cloned(),
                    ),
                );
            }
            seen.insert(
                ino,
                Seen {
                    old: old.map(|i| (node_type(i), size_of(i))),
                    new: new.map(|i| (node_type(i), size_of(i))),
                    changed,
                },
            );
        }
        if end == u64::MAX {
            break;
        }
        from = end + 1;
    }
    // Pass 2: both sets of directory entries, merged per directory in name order.
    let mut old_parents = HashMap::new();
    let mut new_parents = HashMap::new();
    let mut removed: BTreeMap<u64, Vec<Name>> = BTreeMap::new();
    let mut added: BTreeMap<u64, Vec<Name>> = BTreeMap::new();
    // Names kept by objects that differ.
    let mut kept: Vec<(Name, u64)> = Vec::new();
    let (mut old_entries, mut new_entries, mut raw) = (0u64, 0u64, 0usize);
    let max_raw = limits.max_changes.saturating_mul(2);
    for (&dir, (old, new)) in &dirs {
        budget.check_time()?;
        let mut olds = Entries::frozen(*old);
        let mut news = Entries::live(live, new.as_deref());
        let mut o = olds.next(&budget)?;
        let mut n = news.next(&budget)?;
        loop {
            let order = match (&o, &n) {
                (None, None) => break,
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some((a, _)), Some((b, _))) => a.cmp(b),
            };
            let take_old = if order.is_le() { o.take() } else { None };
            let take_new = if order.is_ge() { n.take() } else { None };
            if let Some((name, ino)) = &take_old {
                old_entries += 1;
                if seen
                    .get(ino)
                    .is_some_and(|s| s.old.is_some_and(|(t, _)| t == NodeType::Dir))
                {
                    old_parents.insert(*ino, (dir, name.clone()));
                }
                o = olds.next(&budget)?;
            }
            if let Some((name, ino)) = &take_new {
                new_entries += 1;
                if seen
                    .get(ino)
                    .is_some_and(|s| s.new.is_some_and(|(t, _)| t == NodeType::Dir))
                {
                    new_parents.insert(*ino, (dir, name.clone()));
                }
                n = news.next(&budget)?;
            }
            if old_entries > limits.max_entries || new_entries > limits.max_entries {
                return Err(too_big(format!(
                    "the filesystem has more than {} directory entries; refused",
                    limits.max_entries
                )));
            }
            match (take_old, take_new) {
                (Some((name, a)), Some((_, b))) if a == b => {
                    if seen.get(&a).is_some_and(|s| !s.changed.is_empty()) {
                        kept.push(((dir, name), a));
                        raw += 1;
                    }
                }
                (a, b) => {
                    if let Some((name, ino)) = a {
                        removed.entry(ino).or_default().push((dir, name));
                        raw += 1;
                    }
                    if let Some((name, ino)) = b {
                        added.entry(ino).or_default().push((dir, name));
                        raw += 1;
                    }
                }
            }
            if raw > max_raw {
                return Err(budget.too_many());
            }
        }
    }

    // Pass 3: classify names and render paths.
    let mut old_paths = Paths::new(old_parents);
    let mut new_paths = Paths::new(new_parents);
    let mut path_bytes = 0usize;
    let mut charge = |p: &str| {
        path_bytes += p.len();
        if path_bytes > limits.max_path_bytes {
            return Err(too_big(format!(
                "the changed paths add up to more than {} bytes; refused",
                limits.max_path_bytes
            )));
        }
        Ok(())
    };
    // Changes to names the workspace has, and deletions keyed by their snapshot path.
    let mut out: Vec<FsChange> = Vec::new();
    let mut gone: BTreeMap<String, FsChange> = BTreeMap::new();
    let entry = |s: &Seen, ino: u64, path: String, change: ChangeKind| {
        let side = if change == ChangeKind::Deleted {
            s.old
        } else {
            s.new
        };
        let (node_type, size) = side.unwrap_or((NodeType::File, None));
        FsChange {
            path,
            change,
            node_type,
            from: None,
            changed: Vec::new(),
            replaced: false,
            previous_type: None,
            object_in_both: s.old.is_some() && s.new.is_some(),
            ino,
            size,
        }
    };
    let without_ctime = |c: &[ChangedField]| {
        c.iter()
            .copied()
            .filter(|f| *f != ChangedField::Ctime)
            .collect::<Vec<_>>()
    };
    let moved: BTreeSet<u64> = removed.keys().chain(added.keys()).copied().collect();
    for (i, &ino) in moved.iter().enumerate() {
        if i % BATCH == 0 {
            budget.check_time()?;
        }
        let s = seen
            .get(&ino)
            .ok_or_else(|| MetaError::Store(format!("an entry names the missing inode {ino}")))?;
        let lost = removed.get(&ino).map_or(&[][..], Vec::as_slice);
        let gained = added.get(&ino).map_or(&[][..], Vec::as_slice);
        if let ([from], [to]) = (lost, gained) {
            if s.old.is_some() && s.new.is_some() {
                let from = old_paths.name(from)?;
                let path = new_paths.name(to)?;
                charge(&from)?;
                charge(&path)?;
                let mut c = entry(s, ino, path, ChangeKind::Renamed);
                c.from = Some(from);
                // The rename itself stamps ctime.
                c.changed = without_ctime(&s.changed);
                out.push(c);
                continue;
            }
        }
        for name in gained {
            let path = new_paths.name(name)?;
            charge(&path)?;
            let mut c = entry(s, ino, path, ChangeKind::Created);
            // The new name explains the ctime.
            c.changed = without_ctime(&s.changed);
            out.push(c);
        }
        for name in lost {
            let path = old_paths.name(name)?;
            charge(&path)?;
            gone.insert(path.clone(), entry(s, ino, path, ChangeKind::Deleted));
        }
    }
    for (i, (name, ino)) in kept.iter().enumerate() {
        if i % BATCH == 0 {
            budget.check_time()?;
        }
        let Some(s) = seen.get(ino) else { continue };
        // A ctime that only moved because another of the object's names did.
        if s.changed == [ChangedField::Ctime] && moved.contains(ino) {
            continue;
        }
        let path = new_paths.name(name)?;
        charge(&path)?;
        let mut c = entry(s, *ino, path, ChangeKind::Modified);
        c.changed = s.changed.clone();
        out.push(c);
    }
    if let Some(s) = seen.get(&ROOT_INO).filter(|s| !s.changed.is_empty()) {
        let mut c = entry(s, ROOT_INO, "/".into(), ChangeKind::Modified);
        c.changed = s.changed.clone();
        out.push(c);
    }

    // A path deleted and then filled again is one change to that path.
    for c in out
        .iter_mut()
        .filter(|c| matches!(c.change, ChangeKind::Created | ChangeKind::Renamed))
    {
        // The same object back under the same path is left as the two facts it is: the
        // directory above it may have moved away, and what moved with it did not include this.
        if gone.get(&c.path).is_none_or(|old| old.ino == c.ino) {
            continue;
        }
        if let Some(old) = gone.remove(&c.path) {
            c.replaced = true;
            c.previous_type = (old.node_type != c.node_type).then_some(old.node_type);
            if c.change == ChangeKind::Created {
                c.change = ChangeKind::Modified;
                c.changed = Vec::new();
            }
        }
    }
    out.extend(gone.into_values());
    if out.len() > limits.max_changes {
        return Err(budget.too_many());
    }
    out.sort_by(|a, b| (&a.path, a.change).cmp(&(&b.path, b.change)));
    Ok(out)
}

/// A finished listing.
struct Listing {
    fs: String,
    base_snapshot: String,
    as_of_index: u64,
    computed_unix_ms: u64,
    summary: ChangeSummary,
    changes: Vec<FsChange>,
}

impl Listing {
    fn page(&self, id: &str, offset: usize, limit: usize) -> FsChangesPage {
        let start = offset.min(self.changes.len());
        let end = start.saturating_add(limit).min(self.changes.len());
        FsChangesPage {
            fs: self.fs.clone(),
            base_snapshot: self.base_snapshot.clone(),
            as_of_index: self.as_of_index,
            computed_unix_ms: self.computed_unix_ms,
            total: self.changes.len() as u64,
            summary: self.summary,
            changes: self.changes[start..end].to_vec(),
            next: (end < self.changes.len()).then(|| format!("{id}.{end}")),
        }
    }
}

struct Cached {
    id: String,
    at: Instant,
    listing: Arc<Listing>,
}

/// Oldest first, shared by every engine of the process.
static CACHE: Mutex<Vec<Cached>> = Mutex::new(Vec::new());
static RUNNING: AtomicUsize = AtomicUsize::new(0);

fn keep(listing: Arc<Listing>) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    cache.retain(|c| c.at.elapsed() < CACHE_TTL);
    while cache.iter().filter(|c| c.listing.fs == listing.fs).count() >= CACHED_PER_FS {
        if let Some(i) = cache.iter().position(|c| c.listing.fs == listing.fs) {
            cache.remove(i);
        }
    }
    while !cache.is_empty()
        && (cache.len() >= CACHED_LISTINGS
            || cache.iter().map(|c| c.listing.changes.len()).sum::<usize>() + listing.changes.len()
                > CACHED_CHANGES)
    {
        cache.remove(0);
    }
    cache.push(Cached {
        id: id.clone(),
        at: Instant::now(),
        listing,
    });
    id
}

fn kept_listing(id: &str, fs: &str) -> Option<Arc<Listing>> {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    cache.retain(|c| c.at.elapsed() < CACHE_TTL);
    cache
        .iter()
        .find(|c| c.id == id && c.listing.fs == fs)
        .map(|c| c.listing.clone())
}

/// One of the [`MAX_CONCURRENT`] computations.
struct Running;

impl Running {
    fn start() -> Result<Self, NativeError> {
        if RUNNING.fetch_add(1, Ordering::SeqCst) >= MAX_CONCURRENT {
            RUNNING.fetch_sub(1, Ordering::SeqCst);
            return Err(
                MetaError::Busy("too many change listings are being computed".into()).into(),
            );
        }
        Ok(Self)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::SeqCst);
    }
}

impl NativeEngine {
    /// The changes of filesystem `fs` since the snapshot it was cloned from, in path order, at most
    /// `limit` per page. Without a `cursor` the listing is computed from the catalog as it is now;
    /// a cursor continues that listing, unchanged by later writes.
    pub fn fs_changes(
        &self,
        fs: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<FsChangesPage, NativeError> {
        self.fs_changes_within(fs, cursor, limit, ChangeLimits::default())
    }

    /// [`Self::fs_changes`] with its own limits.
    pub fn fs_changes_within(
        &self,
        fs: &str,
        cursor: Option<&str>,
        limit: usize,
        limits: ChangeLimits,
    ) -> Result<FsChangesPage, NativeError> {
        if fs.contains('@') {
            return Err(NativeError::Invalid(
                "changes are listed for a filesystem, not for a snapshot of one".into(),
            ));
        }
        let limit = limit.clamp(1, MAX_CHANGES_LIMIT);
        if let Some(cursor) = cursor {
            let (id, offset) = cursor
                .split_once('.')
                .and_then(|(id, o)| Some((id, o.parse::<usize>().ok()?)))
                .ok_or_else(|| NativeError::Invalid(format!("malformed cursor {cursor:?}")))?;
            self.with_catalog(|c| c.filesystem(fs).map(|_| ()))??;
            let listing = kept_listing(id, fs).ok_or_else(|| {
                refused(
                    "the cursor has expired or belongs to another listing; \
                     start again without a cursor",
                )
            })?;
            return Ok(listing.page(id, offset, limit));
        }
        let _running = Running::start()?;
        let started = Instant::now();
        // One look at the catalog fixes the live tree: the table's clone shares its inodes and
        // reads the rest from the store as of the same checkpoint.
        let (base, created_ns, live, as_of_index) =
            self.with_catalog(|c| -> Result<_, MetaError> {
                let f = c.filesystem(fs)?;
                let base = f.source_snapshot.clone().ok_or_else(|| {
                    MetaError::Unsupported(format!(
                        "filesystem {fs} was not cloned from a snapshot, so there is nothing to \
                     compare it with"
                    ))
                })?;
                let s = c.fs_snapshots.get(&base).ok_or_else(|| {
                    MetaError::Unsupported(format!(
                    "snapshot {base}, which filesystem {fs} was cloned from, has been deleted; \
                     the changes since it cannot be listed"
                ))
                })?;
                if s.tree.next_ino > f.next_ino {
                    return Err(MetaError::Unsupported(format!(
                        "snapshot {base} is not the one filesystem {fs} was cloned from"
                    )));
                }
                if f.inodes.len().max(s.tree.inodes.len()) > limits.max_inodes {
                    return Err(MetaError::TooBig(format!(
                        "the filesystem has more than {} inodes; refused",
                        limits.max_inodes
                    )));
                }
                Ok((base, s.created_ns, f.inodes.clone(), c.applied_index))
            })??;
        // A snapshot never changes, so it can be read a batch per lock.
        let mut frozen: Vec<Arc<Inode>> = Vec::new();
        let mut from = 0;
        loop {
            let batch = self.with_catalog(|c| -> Result<_, MetaError> {
                c.fs_snapshots
                    .get(&base)
                    .filter(|s| s.created_ns == created_ns)
                    .ok_or_else(|| {
                        MetaError::Unsupported(format!(
                            "snapshot {base} was deleted while the changes were being listed"
                        ))
                    })?
                    .tree
                    .inodes
                    .page_full(from, BATCH)
            })??;
            let last = batch.last().map(|i| i.ino);
            let full = batch.len() == BATCH;
            frozen.extend(batch);
            match last {
                Some(l) if full => from = l + 1,
                _ => break,
            }
        }
        let remaining = ChangeLimits {
            deadline: limits.deadline.saturating_sub(started.elapsed()),
            ..limits
        };
        let changes = compute(&frozen, &live, remaining)?;
        let mut summary = ChangeSummary::default();
        for c in &changes {
            match c.change {
                ChangeKind::Created => summary.created += 1,
                ChangeKind::Modified => summary.modified += 1,
                ChangeKind::Deleted => summary.deleted += 1,
                ChangeKind::Renamed => summary.renamed += 1,
            }
        }
        let listing = Arc::new(Listing {
            fs: fs.into(),
            base_snapshot: base,
            as_of_index,
            computed_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
            summary,
            changes,
        });
        if listing.changes.len() <= limit {
            return Ok(listing.page("", 0, limit));
        }
        let id = keep(listing.clone());
        Ok(listing.page(&id, 0, limit))
    }
}

#[cfg(test)]
mod tests;
