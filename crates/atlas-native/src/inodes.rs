// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! A filesystem's inode table. Inodes are shared (`Arc`): a read hands out a reference without
//! holding the table, and a snapshot or clone shares every inode until one side changes it.
//! Changes are tracked for the catalog store like the catalog's other maps.
//!
//! Once the catalog store holds a filesystem, its table is paged: memory keeps only the inodes
//! changed since the last checkpoint, and every other read goes to the store as of that
//! checkpoint, through a bounded cache ([`StoreSnapshot`]).
//!
//! A directory's entries are paged the same way ([`DirEntries`]): once the store holds a
//! directory, its inode keeps only the entries changed since the last checkpoint and their count,
//! and the table reads the rest from the store one name or one page at a time.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, io,
    ops::Bound,
    sync::Arc,
};

use serde::{
    de::{MapAccess, Visitor},
    Deserialize, Deserializer, Serialize, Serializer,
};

use crate::{
    metadata::MetaError,
    namespace::{FsId, Inode, InodeKind},
    store::StoreSnapshot,
    tracked::Tracked,
};

#[derive(Debug, Clone, Default)]
pub struct InodeTable {
    /// Every inode of an unpaged table; the inodes changed since the last checkpoint of a
    /// paged one (an inode removed since is absent here but in the change record).
    map: Tracked<u64, Arc<Inode>>,
    len: u64,
    paged: Option<Paged>,
}

#[derive(Clone)]
struct Paged {
    fs: FsId,
    store: Arc<StoreSnapshot>,
}

impl fmt::Debug for Paged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Paged").field("fs", &self.fs).finish()
    }
}

fn store_err(e: io::Error) -> MetaError {
    MetaError::Store(e.to_string())
}

/// A directory's entries. Until the catalog store holds the directory, all of them are in
/// memory; after that only those changed since the last checkpoint are (an entry removed since is
/// absent from `map` but in `changed`), and [`InodeTable::entry`] / [`InodeTable::entries`] read
/// the rest from the store. Read them only through the table the inode came from.
#[derive(Debug, Clone, Default)]
pub struct DirEntries {
    map: BTreeMap<String, u64>,
    changed: BTreeSet<String>,
    len: u64,
    stored: bool,
}

impl DirEntries {
    /// `len` entries, all in the store.
    pub(crate) fn stored(len: u64) -> Self {
        Self {
            len,
            stored: true,
            ..Self::default()
        }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the store holds entries not in memory.
    pub(crate) fn is_stored(&self) -> bool {
        self.stored
    }

    /// The entries in memory: every entry unless [`Self::is_stored`].
    pub(crate) fn in_memory(&self) -> impl Iterator<Item = (&String, &u64)> + '_ {
        self.map.iter()
    }

    /// What memory knows about `name`: `Some(None)` if it is certainly absent, `None` if only the
    /// store can tell.
    fn known(&self, name: &str) -> Option<Option<u64>> {
        if let Some(i) = self.map.get(name) {
            return Some(Some(*i));
        }
        (!self.stored || self.changed.contains(name)).then_some(None)
    }

    /// Sets or removes `name`, which `existed` before.
    fn set(&mut self, name: &str, ino: Option<u64>, existed: bool) {
        match ino {
            Some(i) => {
                self.map.insert(name.to_string(), i);
                self.len += u64::from(!existed);
            }
            None => {
                self.map.remove(name);
                self.len -= u64::from(existed);
            }
        }
        self.changed.insert(name.to_string());
    }

    /// Entries changed since the last checkpoint, with their new value (`None`: removed).
    pub(crate) fn changes(&self) -> impl Iterator<Item = (&str, Option<u64>)> + '_ {
        self.changed
            .iter()
            .map(|n| (n.as_str(), self.map.get(n).copied()))
    }

    fn settled(&self) -> bool {
        self.stored && self.map.is_empty() && self.changed.is_empty()
    }

    /// Drops the entries from memory once the store holds all of them.
    fn settle(&mut self) {
        self.map.clear();
        self.changed.clear();
        self.stored = true;
    }
}

impl From<BTreeMap<String, u64>> for DirEntries {
    fn from(map: BTreeMap<String, u64>) -> Self {
        Self {
            len: map.len() as u64,
            map,
            changed: BTreeSet::new(),
            stored: false,
        }
    }
}

impl FromIterator<(String, u64)> for DirEntries {
    fn from_iter<I: IntoIterator<Item = (String, u64)>>(iter: I) -> Self {
        BTreeMap::from_iter(iter).into()
    }
}

/// Equality is over the entries in memory and the count; compare paged directories through
/// [`InodeTable::full`].
impl PartialEq for DirEntries {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.stored == other.stored && self.map == other.map
    }
}
impl Eq for DirEntries {}

/// A directory the store holds can't be serialized from its inode alone: go through
/// [`InodeTable::full`].
impl Serialize for DirEntries {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.stored {
            return Err(serde::ser::Error::custom(
                "a paged directory's entries are read through its inode table",
            ));
        }
        self.map.serialize(s)
    }
}

/// A map of every entry, or (in an inode record of the catalog store) the count of entries the
/// store holds.
impl<'de> Deserialize<'de> for DirEntries {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = DirEntries;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a map of directory entries or an entry count")
            }
            fn visit_u64<E: serde::de::Error>(self, n: u64) -> Result<DirEntries, E> {
                Ok(DirEntries::stored(n))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<DirEntries, A::Error> {
                let mut map = BTreeMap::new();
                while let Some((k, v)) = m.next_entry::<String, u64>()? {
                    map.insert(k, v);
                }
                Ok(map.into())
            }
        }
        d.deserialize_any(V)
    }
}

fn unpaged_dir(ino: u64) -> MetaError {
    MetaError::Store(format!(
        "directory {ino} keeps its entries in a store its table doesn't page"
    ))
}

impl InodeTable {
    /// A table of `len` inodes that the store holds.
    pub(crate) fn paged(fs: FsId, store: Arc<StoreSnapshot>, len: u64) -> Self {
        Self {
            map: Tracked::default(),
            len,
            paged: Some(Paged { fs, store }),
        }
    }

    /// The unchanged store behind `ino`, unless the table changed it since the checkpoint.
    fn behind(&self, ino: u64) -> Option<&Paged> {
        self.paged
            .as_ref()
            .filter(|_| !self.map.touched().contains(&ino))
    }

    pub fn get(&self, ino: u64) -> Result<Option<Arc<Inode>>, MetaError> {
        if let Some(i) = self.map.get(&ino) {
            return Ok(Some(i.clone()));
        }
        match self.behind(ino) {
            Some(p) => p.store.get(&p.fs, ino).map_err(store_err),
            None => Ok(None),
        }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Brings `ino` into `map` to be changed; false if there is no such inode.
    fn page_in(&mut self, ino: u64) -> Result<bool, MetaError> {
        if self.map.contains_key(&ino) {
            return Ok(true);
        }
        let Some(p) = self.behind(ino) else {
            return Ok(false);
        };
        match p.store.take(&p.fs, ino).map_err(store_err)? {
            Some(i) => {
                self.map.insert_quietly(ino, i);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// The inode to edit in place; copied first if a snapshot or a reader still shares it.
    pub fn get_mut(&mut self, ino: u64) -> Result<Option<&mut Inode>, MetaError> {
        if !self.page_in(ino)? {
            return Ok(None);
        }
        Ok(self.map.get_mut(&ino).map(Arc::make_mut))
    }

    /// Adds a new inode (inode numbers are never reused).
    pub fn insert(&mut self, inode: Inode) {
        if self.map.insert(inode.ino, Arc::new(inode)).is_none() {
            self.len += 1;
        }
    }

    pub fn remove(&mut self, ino: u64) -> Result<Option<Inode>, MetaError> {
        if !self.page_in(ino)? {
            return Ok(None);
        }
        let i = self.map.remove(&ino).map(Arc::unwrap_or_clone);
        if i.is_some() {
            self.len -= 1;
        }
        Ok(i)
    }

    /// The inode `name` in directory `dir` (an inode of this table) names, if any.
    pub fn entry(&self, dir: &Inode, name: &str) -> Result<Option<u64>, MetaError> {
        let InodeKind::Dir { entries, .. } = &dir.kind else {
            return Ok(None);
        };
        if let Some(known) = entries.known(name) {
            return Ok(known);
        }
        let p = self.paged.as_ref().ok_or_else(|| unpaged_dir(dir.ino))?;
        p.store.entry(&p.fs, dir.ino, name).map_err(store_err)
    }

    /// Up to `limit` entries of directory `dir` (an inode of this table) named after `after`,
    /// in name order.
    pub fn entries(
        &self,
        dir: &Inode,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, u64)>, MetaError> {
        let InodeKind::Dir { entries, .. } = &dir.kind else {
            return Ok(Vec::new());
        };
        let lower = after.map_or(Bound::Unbounded, Bound::Excluded);
        let mut out: BTreeMap<String, u64> = entries
            .map
            .range::<str, _>((lower, Bound::Unbounded))
            .take(limit)
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        if entries.stored {
            let p = self.paged.as_ref().ok_or_else(|| unpaged_dir(dir.ino))?;
            out.extend(
                p.store
                    .entries(&p.fs, dir.ino, after, limit, |n| {
                        entries.changed.contains(n)
                    })
                    .map_err(store_err)?,
            );
        }
        Ok(out.into_iter().take(limit).collect())
    }

    /// Sets (`Some`) or removes (`None`) the entry `name` of directory `dir`; returns what it
    /// named before.
    pub(crate) fn set_entry(
        &mut self,
        dir: u64,
        name: &str,
        ino: Option<u64>,
    ) -> Result<Option<u64>, MetaError> {
        let Some(d) = self.get(dir)? else {
            return Err(MetaError::NotFound(format!("inode {dir}")));
        };
        let old = self.entry(&d, name)?;
        drop(d);
        match self.get_mut(dir)?.map(|i| &mut i.kind) {
            Some(InodeKind::Dir { entries, .. }) => entries.set(name, ino, old.is_some()),
            _ => return Err(MetaError::NotDir(format!("inode {dir}"))),
        }
        Ok(old)
    }

    /// `i` with every directory entry in memory, ready to leave this table.
    pub fn full(&self, i: Arc<Inode>) -> Result<Arc<Inode>, MetaError> {
        match &i.kind {
            InodeKind::Dir { entries, parent } if entries.stored => {
                let all = self.entries(&i, None, usize::MAX)?;
                Ok(Arc::new(Inode {
                    kind: InodeKind::Dir {
                        parent: *parent,
                        entries: all.into_iter().collect(),
                    },
                    ..(*i).clone()
                }))
            }
            _ => Ok(i),
        }
    }

    /// [`Self::scan`] with every directory entry in memory.
    pub fn scan_full(&self) -> Result<Vec<Arc<Inode>>, MetaError> {
        self.scan()?.into_iter().map(|i| self.full(i)).collect()
    }

    /// [`Self::page`] with every directory entry in memory.
    pub fn page_full(&self, from: u64, limit: usize) -> Result<Vec<Arc<Inode>>, MetaError> {
        self.page(from, limit)?
            .into_iter()
            .map(|i| self.full(i))
            .collect()
    }

    /// Every inode, in inode order (read from the store for a paged table, bypassing its cache).
    /// A directory's entries may still be in the store: see [`Self::scan_full`].
    pub fn scan(&self) -> Result<Vec<Arc<Inode>>, MetaError> {
        let Some(p) = &self.paged else {
            return Ok(self.map.values().cloned().collect());
        };
        let mut all = BTreeMap::new();
        for ino in p.store.inos(&p.fs).map_err(store_err)? {
            if self.map.touched().contains(&ino) {
                continue;
            }
            if let Some(i) = p.store.peek(&p.fs, ino).map_err(store_err)? {
                all.insert(ino, i);
            }
        }
        all.extend(self.map.iter().map(|(k, v)| (*k, v.clone())));
        Ok(all.into_values().collect())
    }

    /// Up to `limit` inodes numbered `from` or more, in inode order (bypassing the cache).
    pub fn page(&self, from: u64, limit: usize) -> Result<Vec<Arc<Inode>>, MetaError> {
        let mut out: BTreeMap<u64, Arc<Inode>> = self
            .map
            .range(from..)
            .take(limit)
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        if let Some(p) = &self.paged {
            let touched = self.map.touched();
            for ino in p
                .store
                .inos_from(&p.fs, from, limit, |i| touched.contains(&i))
                .map_err(store_err)?
            {
                if let Some(i) = p.store.peek(&p.fs, ino).map_err(store_err)? {
                    out.insert(ino, i);
                }
            }
        }
        Ok(out.into_values().take(limit).collect())
    }

    /// The same inodes in memory, apart from the store and with no changes recorded (a
    /// snapshot's frozen tree).
    pub fn detached(&self) -> Result<Self, MetaError> {
        Ok(self
            .scan_full()?
            .into_iter()
            .map(Arc::unwrap_or_clone)
            .collect())
    }

    /// Inodes changed since [`Self::clear_changes`].
    pub(crate) fn touched(&self) -> impl Iterator<Item = u64> + '_ {
        self.map.touched().iter().copied()
    }

    /// Whether `ino` was inserted or removed (rather than edited in place) since the last
    /// [`Self::clear_changes`].
    pub(crate) fn replaced(&self, ino: u64) -> bool {
        self.map.replaced().contains(&ino)
    }

    /// Forgets the changes. Changed directory entries stay in memory until [`Self::attach`].
    pub(crate) fn clear_changes(&mut self) {
        self.map.clear_changes();
    }

    /// Pages the table on `store`, which holds all of it, handing the inodes in memory to its
    /// cache with their directory entries left to the store. Only after [`Self::clear_changes`].
    pub(crate) fn attach(&mut self, fs: &FsId, store: Arc<StoreSnapshot>) {
        debug_assert!(self.map.touched().is_empty());
        for mut i in std::mem::take(&mut self.map).into_values() {
            if matches!(&i.kind, InodeKind::Dir { entries, .. } if !entries.settled()) {
                if let InodeKind::Dir { entries, .. } = &mut Arc::make_mut(&mut i).kind {
                    entries.settle();
                }
            }
            store.put(fs, i.ino, i);
        }
        self.paged = Some(Paged {
            fs: fs.clone(),
            store,
        });
    }
}

impl FromIterator<Inode> for InodeTable {
    fn from_iter<I: IntoIterator<Item = Inode>>(iter: I) -> Self {
        let map: Tracked<u64, Arc<Inode>> =
            iter.into_iter().map(|i| (i.ino, Arc::new(i))).collect();
        Self {
            len: map.len() as u64,
            map,
            paged: None,
        }
    }
}

/// Equality is over contents (read from the store where paged; a store error is inequality).
impl PartialEq for InodeTable {
    fn eq(&self, other: &Self) -> bool {
        matches!((self.scan_full(), other.scan_full()), (Ok(a), Ok(b)) if a == b)
    }
}
impl Eq for InodeTable {}

impl Serialize for InodeTable {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let all = self.scan_full().map_err(serde::ser::Error::custom)?;
        s.collect_map(all.iter().map(|i| (i.ino, &**i)))
    }
}

impl<'de> Deserialize<'de> for InodeTable {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        BTreeMap::<u64, Inode>::deserialize(d).map(|m| m.into_values().collect())
    }
}
