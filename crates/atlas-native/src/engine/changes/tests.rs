// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The change listing against a model: both trees walked into `path -> inode` maps, and the
//! listing replayed over the old one has to produce the new one, with exactly the reported
//! differences.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::{
    metadata::{Catalog, ExtentRef, MetaCommand, MetaError, ReplicaRef},
    namespace::{FsMeta, FsOp, NodeType, SetAttr, XattrMode},
    store::{CatalogStore, CATALOG_STORE},
};

const GRID: u64 = 4096;
const BASE: &str = "s";
const WORK: &str = "w";

struct H {
    c: Catalog,
    index: u64,
    now: i64,
    /// Checkpointed now and then, so the live table is paged.
    store: Option<(tempfile::TempDir, CatalogStore)>,
}

impl H {
    fn new(paged: bool) -> Self {
        let store = paged.then(|| {
            let td = tempfile::tempdir().unwrap();
            let s = CatalogStore::open(td.path().join(CATALOG_STORE))
                .unwrap()
                .with_cache_inodes(64);
            (td, s)
        });
        let mut h = Self {
            c: Catalog::default(),
            index: 0,
            now: 0,
            store,
        };
        h.ok(FsOp::CreateFs {
            fs: "src".into(),
            name: "src".into(),
            now_ns: 0,
            extent_bytes: None,
        });
        h
    }

    fn run(&mut self, op: FsOp) -> Result<(), MetaError> {
        self.index += 1;
        self.c
            .apply_committed(1, self.index, &MetaCommand::Fs { op })
            .map(|_| ())
    }

    fn ok(&mut self, op: FsOp) {
        self.run(op).expect("op applies");
    }

    fn tick(&mut self) -> i64 {
        self.now += 1;
        self.now
    }

    fn checkpoint(&mut self) {
        if let Some((_, s)) = &self.store {
            s.checkpoint(&mut self.c).unwrap();
        }
    }

    /// Snapshots `src` and clones it as the workspace.
    fn fork(&mut self) {
        let now_ns = self.tick();
        self.ok(FsOp::SnapshotFs {
            id: BASE.into(),
            fs: "src".into(),
            name: BASE.into(),
            now_ns,
        });
        self.ok(FsOp::CloneFs {
            id: WORK.into(),
            name: WORK.into(),
            snapshot_id: BASE.into(),
        });
    }

    fn ino(&self, fs: &str, path: &str) -> u64 {
        let f = &self.c.filesystems[fs];
        path.split('/')
            .filter(|p| !p.is_empty())
            .fold(ROOT_INO, |dir, name| f.lookup(dir, name).unwrap())
    }

    /// The directory inode and last name of `path`.
    fn at<'a>(&self, fs: &str, path: &'a str) -> (u64, &'a str) {
        let (dir, name) = path.rsplit_once('/').expect("an absolute path");
        (self.ino(fs, dir), name)
    }

    fn mk_in(&mut self, fs: &str, parent: u64, name: &str, t: NodeType) -> Result<(), MetaError> {
        let now_ns = self.tick();
        self.run(FsOp::Mknode {
            fs: fs.into(),
            parent,
            name: name.into(),
            op_id: format!("op-{now_ns}"),
            node_type: t,
            target: (t == NodeType::Symlink).then(|| "target".to_string()),
            rdev: 0,
            mode: 0o644,
            create_mode: None,
            uid: 1000,
            gid: 1000,
            now_ns,
        })
    }

    fn mk(&mut self, fs: &str, path: &str, t: NodeType) {
        let (parent, name) = self.at(fs, path);
        self.mk_in(fs, parent, name, t).unwrap();
    }

    fn write_ino(&mut self, fs: &str, ino: u64, off: u64, len: u64) -> Result<(), MetaError> {
        let now_ns = self.tick();
        let id = format!("e{now_ns}");
        self.run(FsOp::InstallFileExtent {
            fs: fs.into(),
            ino,
            logical_offset: off,
            extent: ExtentRef {
                id,
                logical_offset: off,
                payload_len: len as usize,
                raw_len: 0,
                checksum: [0; 32],
                content: None,
                replicas: vec![ReplicaRef {
                    node_id: "n".into(),
                    device_index: 0,
                    offset: now_ns as u64 * GRID,
                }],
                ec: None,
                created_ms: 0,
                object: None,
                delta: false,
            },
            size: off + len,
            now_ns,
            replica: false,
        })
    }

    fn write(&mut self, fs: &str, path: &str) {
        let ino = self.ino(fs, path);
        self.write_ino(fs, ino, 0, 10).unwrap();
    }

    fn unlink(&mut self, fs: &str, path: &str) {
        let (parent, name) = self.at(fs, path);
        let now_ns = self.tick();
        self.ok(FsOp::Unlink {
            fs: fs.into(),
            parent,
            name: name.into(),
            now_ns,
        });
    }

    fn rmdir(&mut self, fs: &str, path: &str) {
        let (parent, name) = self.at(fs, path);
        let now_ns = self.tick();
        self.ok(FsOp::Rmdir {
            fs: fs.into(),
            parent,
            name: name.into(),
            now_ns,
        });
    }

    fn mv(&mut self, fs: &str, from: &str, to: &str) {
        let (parent, name) = self.at(fs, from);
        let (new_parent, new_name) = self.at(fs, to);
        let now_ns = self.tick();
        self.ok(FsOp::Rename {
            fs: fs.into(),
            parent,
            name: name.into(),
            new_parent,
            new_name: new_name.into(),
            now_ns,
        });
    }

    fn link(&mut self, fs: &str, from: &str, to: &str) {
        let ino = self.ino(fs, from);
        let (parent, name) = self.at(fs, to);
        let now_ns = self.tick();
        self.ok(FsOp::Link {
            fs: fs.into(),
            ino,
            parent,
            name: name.into(),
            now_ns,
        });
    }

    fn setattr(&mut self, fs: &str, path: &str, attr: SetAttr) {
        let ino = self.ino(fs, path);
        let now_ns = self.tick();
        self.ok(FsOp::SetAttr {
            fs: fs.into(),
            ino,
            attr,
            now_ns,
        });
    }

    fn setxattr(&mut self, fs: &str, path: &str, name: &str, value: &[u8]) {
        let ino = self.ino(fs, path);
        let now_ns = self.tick();
        self.ok(FsOp::SetXattr {
            fs: fs.into(),
            ino,
            name: name.into(),
            value: value.to_vec(),
            mode: XattrMode::Set,
            now_ns,
        });
    }

    fn listing(&self, limits: ChangeLimits) -> Result<Vec<FsChange>, NativeError> {
        let base = self.c.fs_snapshots[BASE]
            .tree
            .inodes
            .page_full(0, usize::MAX)
            .unwrap();
        compute(&base, &self.c.filesystems[WORK].inodes, limits)
    }

    fn changes(&self) -> Vec<FsChange> {
        let got = self.listing(ChangeLimits::default()).unwrap();
        verify(
            &walk(&self.c.fs_snapshots[BASE].tree),
            &walk(&self.c.filesystems[WORK]),
            &got,
        );
        got
    }

    /// One line per change: what a test expects to read.
    fn lines(&self) -> Vec<String> {
        self.changes().iter().map(line).collect()
    }
}

fn line(c: &FsChange) -> String {
    let mut s = format!("{:?} {}", c.change, c.path).to_lowercase();
    if let Some(f) = &c.from {
        s.push_str(&format!(" from {f}"));
    }
    if c.replaced {
        s.push_str(" replaced");
    }
    if !c.changed.is_empty() {
        let fields: Vec<String> = c
            .changed
            .iter()
            .map(|f| format!("{f:?}").to_lowercase())
            .collect();
        s.push_str(&format!(" [{}]", fields.join(",")));
    }
    s
}

/// Every path of a tree (the root as `""`).
fn walk(f: &FsMeta) -> BTreeMap<String, Arc<Inode>> {
    let mut out = BTreeMap::new();
    let mut todo = vec![(String::new(), ROOT_INO)];
    while let Some((path, ino)) = todo.pop() {
        let i = f.inodes.get(ino).unwrap().unwrap();
        if i.is_dir() {
            for (name, child) in f.inodes.entries(&i, None, usize::MAX).unwrap() {
                todo.push((format!("{path}/{name}"), child));
            }
        }
        out.insert(path, i);
    }
    out
}

/// The differences a listing has to report, restated independently of `compare`.
fn expected(a: &Inode, b: &Inode) -> BTreeSet<ChangedField> {
    use ChangedField::*;
    let mut s = BTreeSet::new();
    let mut add = |yes: bool, f| {
        if yes {
            s.insert(f);
        }
    };
    add(a.mode != b.mode, Mode);
    add(a.uid != b.uid || a.gid != b.gid, Owner);
    add(a.xattrs != b.xattrs, Xattrs);
    if !b.is_dir() {
        add(a.nlink != b.nlink, Links);
        add(a.mtime_ns != b.mtime_ns, Mtime);
        add(a.atime_ns != b.atime_ns, Atime);
        add(a.ctime_ns != b.ctime_ns, Ctime);
    }
    if let (
        InodeKind::File {
            size: x,
            extents: ex,
            ..
        },
        InodeKind::File {
            size: y,
            extents: ey,
            ..
        },
    ) = (&a.kind, &b.kind)
    {
        add(x != y || ex != ey, Content);
        add(x != y, Size);
    }
    s
}

fn parent(path: &str) -> (&str, &str) {
    path.rsplit_once('/').expect("a path below the root")
}

fn verify(
    old: &BTreeMap<String, Arc<Inode>>,
    new: &BTreeMap<String, Arc<Inode>>,
    got: &[FsChange],
) {
    use ChangeKind::*;
    let root = |p: &str| if p == "/" { String::new() } else { p.into() };
    let mut renamed = BTreeMap::new();
    let mut gone = BTreeSet::new();
    let mut fresh = BTreeSet::new();
    let mut modified = BTreeMap::new();
    let mut inos = BTreeSet::new();
    for c in got {
        inos.insert(c.ino);
        let path = root(&c.path);
        if c.change == Deleted {
            assert_eq!(old[&path].ino, c.ino, "{c:?}");
            assert_eq!(node_type(&old[&path]), c.node_type, "{c:?}");
            assert_eq!(
                c.object_in_both,
                new.values().any(|i| i.ino == c.ino),
                "{c:?}"
            );
            assert!(gone.insert(path), "listed twice: {c:?}");
            continue;
        }
        assert_eq!(new[&path].ino, c.ino, "{c:?}");
        assert_eq!(node_type(&new[&path]), c.node_type, "{c:?}");
        assert_eq!(
            c.object_in_both,
            old.values().any(|i| i.ino == c.ino),
            "{c:?}"
        );
        if c.replaced {
            assert_ne!(old[&path].ino, c.ino, "{c:?}");
            assert!(gone.insert(path.clone()), "listed twice: {c:?}");
        }
        match c.change {
            Renamed => {
                // Equal to `path` when the directory above was replaced by another one.
                let from = c.from.clone().expect("a rename says where from");
                assert_eq!(old[&from].ino, c.ino, "{c:?}");
                let mut want = expected(&old[&from], &new[&path]);
                want.remove(&ChangedField::Ctime);
                assert_eq!(c.changed.iter().copied().collect::<BTreeSet<_>>(), want);
                assert!(renamed.insert(from, path).is_none(), "{c:?}");
            }
            Created => {
                assert!(!c.replaced, "{c:?}");
                assert!(fresh.insert(path), "{c:?}");
            }
            Modified if c.replaced => assert!(fresh.insert(path), "{c:?}"),
            Modified => assert!(modified.insert(path, c).is_none(), "{c:?}"),
            Deleted => unreachable!(),
        }
    }

    // Where a snapshot directory's unlisted children are now.
    fn prefix(dir: &str, renamed: &BTreeMap<String, String>, gone: &BTreeSet<String>) -> String {
        if dir.is_empty() {
            String::new()
        } else if let Some(to) = renamed.get(dir) {
            to.clone()
        } else if gone.contains(dir) {
            dir.to_string()
        } else {
            let (up, name) = parent(dir);
            format!("{}/{name}", prefix(up, renamed, gone))
        }
    }

    let mut image = BTreeSet::new();
    for (path, before) in old {
        let to = if path.is_empty() {
            String::new()
        } else if let Some(to) = renamed.get(path) {
            to.clone()
        } else if gone.contains(path) {
            continue;
        } else {
            let (up, name) = parent(path);
            format!("{}/{name}", prefix(up, &renamed, &gone))
        };
        let after = new
            .get(&to)
            .unwrap_or_else(|| panic!("{path} should be at {to}: {got:#?}"));
        assert_eq!(before.ino, after.ino, "{path} -> {to}: {got:#?}");
        assert!(image.insert(to.clone()), "two names became {to}");
        if renamed.contains_key(path) {
            continue;
        }
        let want = expected(before, after);
        match modified.remove(&to) {
            Some(c) => {
                let mut said: BTreeSet<_> = c.changed.iter().copied().collect();
                if want.contains(&ChangedField::Ctime) {
                    said.insert(ChangedField::Ctime);
                }
                assert_eq!(said, want, "{path} -> {to}: {c:?}");
                assert!(!c.changed.is_empty(), "{c:?}");
            }
            None => {
                let mut rest = want.clone();
                rest.remove(&ChangedField::Ctime);
                assert!(rest.is_empty(), "{to} changed {want:?} unlisted: {got:#?}");
                assert!(
                    want.is_empty() || inos.contains(&before.ino),
                    "{to} was touched and nothing says so: {got:#?}"
                );
            }
        }
    }
    assert!(
        modified.is_empty(),
        "modified without an old name: {modified:?}"
    );
    let unexplained: BTreeSet<String> = new
        .keys()
        .filter(|p| !image.contains(*p))
        .cloned()
        .collect();
    assert_eq!(unexplained, fresh, "new paths: {got:#?}");

    // An object that lost one name and gained one is a rename.
    let names = |m: &BTreeMap<String, Arc<Inode>>| {
        let mut out: BTreeMap<u64, Vec<(u64, String)>> = BTreeMap::new();
        for (p, i) in m {
            if !p.is_empty() {
                let (up, name) = parent(p);
                out.entry(i.ino)
                    .or_default()
                    .push((m[up].ino, name.to_string()));
            }
        }
        out
    };
    let (before, after) = (names(old), names(new));
    for (ino, b) in &before {
        let Some(a) = after.get(ino) else { continue };
        let (b_set, a_set): (BTreeSet<_>, BTreeSet<_>) = (b.iter().collect(), a.iter().collect());
        let lost: Vec<_> = b_set.difference(&a_set).collect();
        let gained: Vec<_> = a_set.difference(&b_set).collect();
        if lost.len() == 1 && gained.len() == 1 {
            let path = |m: &BTreeMap<String, Arc<Inode>>, (dir, name): &(u64, String)| {
                let d = m.iter().find(|(_, i)| i.ino == *dir).unwrap().0;
                format!("{d}/{name}")
            };
            let (from, to) = (path(old, lost[0]), path(new, gained[0]));
            assert!(
                got.iter()
                    .any(|c| c.change == Renamed && c.ino == *ino && c.path == to),
                "{from} -> {to} is not reported as a rename: {got:#?}"
            );
        }
    }
}

/// A source tree: `/a`, `/d/b`, `/d/e/c`, `/link` (symlink) and an empty `/empty`.
fn seeded(paged: bool) -> H {
    let mut h = H::new(paged);
    for (p, t) in [
        ("/a", NodeType::File),
        ("/d", NodeType::Dir),
        ("/d/b", NodeType::File),
        ("/d/e", NodeType::Dir),
        ("/d/e/c", NodeType::File),
        ("/link", NodeType::Symlink),
        ("/empty", NodeType::Dir),
    ] {
        h.mk("src", p, t);
    }
    for p in ["/a", "/d/b", "/d/e/c"] {
        h.write("src", p);
    }
    h.checkpoint();
    h.fork();
    h.checkpoint();
    h
}

#[test]
fn an_untouched_clone_has_no_changes_and_the_source_does_not_count() {
    for paged in [false, true] {
        let mut h = seeded(paged);
        assert_eq!(h.lines(), Vec::<String>::new());
        h.write("src", "/a");
        h.mk("src", "/later", NodeType::File);
        h.unlink("src", "/d/b");
        h.checkpoint();
        assert_eq!(h.lines(), Vec::<String>::new());
    }
}

#[test]
fn creates_edits_and_deletes() {
    for paged in [false, true] {
        let mut h = seeded(paged);
        h.mk(WORK, "/d/new", NodeType::Dir);
        h.mk(WORK, "/d/new/f", NodeType::File);
        h.write(WORK, "/a");
        h.unlink(WORK, "/d/e/c");
        h.rmdir(WORK, "/d/e");
        h.checkpoint();
        assert_eq!(
            h.lines(),
            [
                "modified /a [content,mtime,ctime]",
                "deleted /d/e",
                "deleted /d/e/c",
                "created /d/new",
                "created /d/new/f",
            ]
        );
        let got = h.changes();
        assert_eq!(got[0].size, Some(10));
        assert_eq!(got[1].node_type, NodeType::Dir);
        assert!(got.iter().skip(1).all(|c| !c.object_in_both));
    }
}

#[test]
fn a_renamed_directory_stands_for_what_is_under_it() {
    for paged in [false, true] {
        let mut h = seeded(paged);
        h.mv(WORK, "/d", "/moved");
        assert_eq!(h.lines(), ["renamed /moved from /d"]);
        h.checkpoint();
        assert_eq!(h.lines(), ["renamed /moved from /d"]);
        // What changes inside it is listed: new paths in the new place, old ones in the old.
        h.write(WORK, "/moved/e/c");
        h.unlink(WORK, "/moved/b");
        h.mv(WORK, "/a", "/moved/e/a2");
        h.checkpoint();
        assert_eq!(
            h.lines(),
            [
                "deleted /d/b",
                "renamed /moved from /d",
                "renamed /moved/e/a2 from /a",
                "modified /moved/e/c [content,mtime,ctime]",
            ]
        );
        // An empty directory.
        h.mv(WORK, "/empty", "/still-empty");
        assert!(h
            .lines()
            .contains(&"renamed /still-empty from /empty".into()));
    }
}

#[test]
fn renames_replacements_and_swaps() {
    for paged in [false, true] {
        // An editor's atomic save: write a new file, rename it over the old one.
        let mut h = seeded(paged);
        h.mk(WORK, "/a.tmp", NodeType::File);
        h.write(WORK, "/a.tmp");
        h.mv(WORK, "/a.tmp", "/a");
        assert_eq!(h.lines(), ["modified /a replaced"]);

        // Over an existing file.
        let mut h = seeded(paged);
        h.mv(WORK, "/d/b", "/a");
        assert_eq!(h.lines(), ["renamed /a from /d/b replaced"]);

        // Swapped.
        let mut h = seeded(paged);
        h.mv(WORK, "/a", "/t");
        h.mv(WORK, "/d/b", "/a");
        h.mv(WORK, "/t", "/d/b");
        assert_eq!(h.lines(), ["renamed /a from /d/b", "renamed /d/b from /a"]);

        // Moved away and the name used again.
        let mut h = seeded(paged);
        h.mv(WORK, "/a", "/kept");
        h.mk(WORK, "/a", NodeType::Dir);
        assert_eq!(h.lines(), ["created /a", "renamed /kept from /a"]);

        // There and back: only the inode was touched.
        let mut h = seeded(paged);
        h.mv(WORK, "/a", "/t");
        h.mv(WORK, "/t", "/a");
        assert_eq!(h.lines(), ["modified /a [ctime]"]);

        // A file replaced by a directory.
        let mut h = seeded(paged);
        h.unlink(WORK, "/a");
        h.mk(WORK, "/a", NodeType::Dir);
        let got = h.changes();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].previous_type, Some(NodeType::File));
        assert_eq!(got[0].node_type, NodeType::Dir);

        // A directory rebuilt around the file it held.
        let mut h = seeded(paged);
        h.mk(WORK, "/tmp", NodeType::Dir);
        h.mv(WORK, "/d/e/c", "/tmp/c");
        h.rmdir(WORK, "/d/e");
        h.mv(WORK, "/tmp", "/d/e");
        assert_eq!(
            h.lines(),
            ["modified /d/e replaced", "renamed /d/e/c from /d/e/c"]
        );

        // The directory moved away and a new one took its name and one of its files: the file
        // did not move with it.
        let mut h = seeded(paged);
        h.mv(WORK, "/d/e", "/elsewhere");
        h.mk(WORK, "/d/e", NodeType::Dir);
        h.mv(WORK, "/elsewhere/c", "/d/e/c");
        assert_eq!(
            h.lines(),
            [
                "created /d/e",
                "renamed /d/e/c from /d/e/c",
                "renamed /elsewhere from /d/e",
            ]
        );
    }
}

#[test]
fn hard_links() {
    for paged in [false, true] {
        let mut h = seeded(paged);
        h.link(WORK, "/a", "/a2");
        assert_eq!(
            h.lines(),
            ["modified /a [links,ctime]", "created /a2 [links]"]
        );
        assert!(h.changes()[1].object_in_both);

        // Content written through one name shows on both.
        h.write(WORK, "/a2");
        assert_eq!(
            h.lines(),
            [
                "modified /a [content,links,mtime,ctime]",
                "created /a2 [content,links,mtime]",
            ]
        );

        // One of two names removed, relative to a snapshot that had both.
        let mut h = H::new(paged);
        h.mk("src", "/x", NodeType::File);
        h.link("src", "/x", "/y");
        h.fork();
        h.unlink(WORK, "/x");
        h.checkpoint();
        assert_eq!(h.lines(), ["deleted /x", "modified /y [links,ctime]"]);
        assert!(h.changes()[0].object_in_both);

        // One of two names moved: the other did not change.
        let mut h = H::new(paged);
        h.mk("src", "/x", NodeType::File);
        h.link("src", "/x", "/y");
        h.fork();
        h.mv(WORK, "/x", "/z");
        assert_eq!(h.lines(), ["renamed /z from /x"]);

        // Both moved: which went where cannot be told, so nothing is called a rename.
        h.mv(WORK, "/y", "/w");
        assert_eq!(
            h.lines(),
            ["created /w", "deleted /x", "deleted /y", "created /z"]
        );
        assert!(h.changes().iter().all(|c| c.object_in_both));
    }
}

#[test]
fn metadata_only_changes() {
    for paged in [false, true] {
        let mut h = seeded(paged);
        let attr = |f: fn(&mut SetAttr)| {
            let mut a = SetAttr::default();
            f(&mut a);
            a
        };
        h.setxattr(WORK, "/a", "user.tag", b"secret-value");
        h.setattr(WORK, "/d/b", attr(|a| a.mode = Some(0o600)));
        h.setattr(WORK, "/d/e/c", attr(|a| a.uid = Some(7)));
        h.setattr(WORK, "/link", attr(|a| a.atime_ns = Some(99)));
        h.setattr(WORK, "/d", attr(|a| a.mode = Some(0o700)));
        // A directory's times alone are not reported.
        h.setattr(WORK, "/empty", attr(|a| a.mtime_ns = Some(99)));
        h.checkpoint();
        assert_eq!(
            h.lines(),
            [
                "modified /a [xattrs,ctime]",
                "modified /d [mode]",
                "modified /d/b [mode,ctime]",
                "modified /d/e/c [owner,ctime]",
                "modified /link [atime,ctime]",
            ]
        );
        let text = serde_json::to_string(&h.changes()).unwrap();
        assert!(!text.contains("secret-value") && !text.contains("user.tag"));
        assert!(!text.contains("target"), "no symlink targets: {text}");

        h.setattr(WORK, "/", attr(|a| a.gid = Some(5)));
        assert_eq!(h.lines()[0], "modified / [owner]");
    }
}

#[test]
fn truncation_and_retargeting() {
    let mut h = seeded(true);
    let size = |n| SetAttr {
        size: Some(n),
        ..Default::default()
    };
    h.setattr(WORK, "/a", size(3));
    h.setattr(WORK, "/d/b", size(100));
    assert_eq!(
        h.lines(),
        [
            "modified /a [content,size,mtime,ctime]",
            "modified /d/b [content,size,mtime,ctime]",
        ]
    );
    // A symlink cannot be retargeted; replacing it is a different object.
    h.unlink(WORK, "/link");
    h.mk(WORK, "/link", NodeType::Symlink);
    assert!(h.lines().contains(&"modified /link replaced".into()));
}

#[test]
fn limits_refuse_rather_than_cut_short() {
    let mut h = seeded(false);
    for i in 0..20 {
        h.mk(WORK, &format!("/n{i}"), NodeType::File);
    }
    let with = |f: fn(&mut ChangeLimits)| {
        let mut l = ChangeLimits::default();
        f(&mut l);
        l
    };
    assert_eq!(h.listing(with(|l| l.max_changes = 20)).unwrap().len(), 20);
    for (limits, says) in [
        (with(|l| l.max_changes = 19), "more than 19 changes"),
        (with(|l| l.max_changes = 5), "more than 5 changes"),
        (with(|l| l.max_inodes = 10), "more than 10 inodes"),
        (with(|l| l.max_entries = 10), "directory entries"),
        (with(|l| l.max_path_bytes = 20), "more than 20 bytes"),
        (with(|l| l.deadline = Duration::ZERO), "took more than"),
    ] {
        match h.listing(limits) {
            Err(NativeError::Metadata(MetaError::TooBig(m))) => {
                assert!(m.contains(says), "{m}")
            }
            other => panic!("wanted a refusal saying {says:?}, got {other:?}"),
        }
    }
}

/// Directories larger than a read batch, paged, with entries changed since the checkpoint.
#[test]
fn big_directories_are_read_in_batches() {
    let mut h = H::new(true);
    h.mk("src", "/big", NodeType::Dir);
    let big = h.ino("src", "/big");
    let n = BATCH * 2 + 100;
    for i in 0..n {
        h.mk_in("src", big, &format!("f{i:05}"), NodeType::File)
            .unwrap();
    }
    h.checkpoint();
    h.fork();
    h.checkpoint();
    assert_eq!(h.lines(), Vec::<String>::new());
    for i in (0..n).step_by(2) {
        h.unlink(WORK, &format!("/big/f{i:05}"));
    }
    h.mv(WORK, "/big/f00001", "/big/g");
    let big = h.ino(WORK, "/big");
    h.mk_in(WORK, big, "a-first", NodeType::File).unwrap();
    let check = |h: &H| {
        let got = h.changes();
        assert_eq!(got.len(), n.div_ceil(2) + 2);
        assert!(got.windows(2).all(|w| w[0].path <= w[1].path));
    };
    check(&h);
    h.checkpoint();
    check(&h);
}

struct Rng(u64);

impl Rng {
    fn next(&mut self, m: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % m
    }

    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.next(v.len() as u64) as usize]
    }
}

/// One random operation on `fs`; those the filesystem refuses change nothing.
fn random_op(h: &mut H, rng: &mut Rng, fs: &str) {
    let tree = walk(&h.c.filesystems[fs]);
    let dirs: Vec<u64> = tree
        .values()
        .filter(|i| i.is_dir())
        .map(|i| i.ino)
        .collect();
    let all: Vec<u64> = tree.values().map(|i| i.ino).collect();
    let entries: Vec<(u64, String, u64, bool)> = tree
        .iter()
        .filter(|(p, _)| !p.is_empty())
        .map(|(p, i)| {
            let (up, name) = parent(p);
            (tree[up].ino, name.to_string(), i.ino, i.is_dir())
        })
        .collect();
    // Few names, so that operations collide and replace one another.
    let name = format!("n{}", rng.next(5));
    let dir = *rng.pick(&dirs);
    let ino = *rng.pick(&all);
    let now_ns = h.tick();
    let fs_id = fs.to_string();
    let op = match rng.next(11) {
        0 | 1 => {
            let t = *rng.pick(&[
                NodeType::File,
                NodeType::File,
                NodeType::Dir,
                NodeType::Symlink,
                NodeType::Fifo,
            ]);
            let _ = h.mk_in(fs, dir, &name, t);
            return;
        }
        2 | 3 if !entries.is_empty() => {
            let (parent, name, _, is_dir) = rng.pick(&entries).clone();
            if is_dir {
                FsOp::Rmdir {
                    fs: fs_id,
                    parent,
                    name,
                    now_ns,
                }
            } else {
                FsOp::Unlink {
                    fs: fs_id,
                    parent,
                    name,
                    now_ns,
                }
            }
        }
        4 | 5 if !entries.is_empty() => {
            let (parent, old, ..) = rng.pick(&entries).clone();
            FsOp::Rename {
                fs: fs_id,
                parent,
                name: old,
                new_parent: dir,
                new_name: name,
                now_ns,
            }
        }
        6 => FsOp::Link {
            fs: fs_id,
            ino,
            parent: dir,
            name,
            now_ns,
        },
        7 => {
            let mut attr = SetAttr::default();
            match rng.next(5) {
                0 => attr.mode = Some(*rng.pick(&[0o644, 0o600])),
                1 => attr.uid = Some(*rng.pick(&[1000, 7])),
                2 => attr.size = Some(rng.next(30)),
                3 => attr.mtime_ns = Some(now_ns),
                _ => attr.atime_ns = Some(now_ns),
            }
            FsOp::SetAttr {
                fs: fs_id,
                ino,
                attr,
                now_ns,
            }
        }
        8 => FsOp::SetXattr {
            fs: fs_id,
            ino,
            name: "user.k".into(),
            value: vec![rng.next(2) as u8],
            mode: XattrMode::Set,
            now_ns,
        },
        9 => FsOp::RemoveXattr {
            fs: fs_id,
            ino,
            name: "user.k".into(),
            now_ns,
        },
        _ => {
            let off = rng.next(2) * GRID;
            let _ = h.write_ino(fs, ino, off, 1 + rng.next(20));
            return;
        }
    };
    let _ = h.run(op);
}

#[test]
fn random_workloads_match_the_model() {
    let mut kinds = BTreeSet::new();
    let (mut replaced, mut renames, mut linked) = (0, 0, 0);
    for seed in 0..300u64 {
        let paged = seed % 3 == 0;
        let mut h = H::new(paged);
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ (seed + 1).wrapping_mul(0x2545_f491_4f6c_dd1d));
        for _ in 0..rng.next(60) {
            random_op(&mut h, &mut rng, "src");
        }
        h.checkpoint();
        h.fork();
        for step in 0..1 + rng.next(50) {
            random_op(&mut h, &mut rng, WORK);
            if rng.next(9) == 0 {
                h.checkpoint();
            }
            if rng.next(11) == 0 {
                random_op(&mut h, &mut rng, "src");
            }
            if step % 7 == 6 {
                h.changes();
            }
        }
        for c in h.changes() {
            kinds.insert(c.change);
            replaced += u32::from(c.replaced);
            renames += u32::from(c.change == ChangeKind::Renamed);
            linked += u32::from(c.change == ChangeKind::Created && c.object_in_both);
        }
    }
    // The workloads actually reach the cases the listing distinguishes.
    assert_eq!(kinds.len(), 4);
    assert!(
        replaced > 10 && renames > 50 && linked > 10,
        "replaced={replaced} renames={renames} linked={linked}"
    );
}
