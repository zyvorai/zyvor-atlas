// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! A rename applied only if its destination is still what the caller read: one replicated
//! compare-and-swap, which any number of clients can use to publish a file under a condition
//! (the S3 gateway's conditional writes).

use serde::{Deserialize, Serialize};

use super::{check_name, FsId, FsOp, Inode};
use crate::{
    metadata::{Catalog, ExtentId, MetaError},
    raft::CAP_RENAME_IF,
};

/// What [`ConditionalRename`] must find at its destination when it applies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RenameExpect {
    /// No entry.
    Absent,
    /// Inode `ino` with the size, modification time and extended attribute `xattr` (`value`;
    /// `None`: unset) it was read with.
    Unchanged {
        ino: u64,
        size: u64,
        mtime_ns: i64,
        xattr: String,
        #[serde(default)]
        value: Option<Vec<u8>>,
    },
}

/// Renames `name` (which must still name inode `ino`) to `new_name`, if every condition holds.
/// A repeat of one that already applied, finding `ino` at the destination, does nothing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConditionalRename {
    /// Set by the server from the request's path.
    #[serde(default)]
    pub fs: FsId,
    pub parent: u64,
    pub name: String,
    pub ino: u64,
    pub new_parent: u64,
    pub new_name: String,
    /// `None`: any destination `rename(2)` allows.
    #[serde(default)]
    pub expect: Option<RenameExpect>,
    /// Refuse (`Locked`) to replace a destination whose extended attribute of this name holds a
    /// decimal Unix time (seconds) later than `now_ns`.
    #[serde(default)]
    pub retain_xattr: Option<String>,
    /// The proposer's clock, so every replica judges retention at the same instant.
    #[serde(default)]
    pub now_ns: i64,
}

fn retained_until(i: &Inode, xattr: &str) -> u64 {
    i.xattrs
        .get(xattr)
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

impl Catalog {
    pub(super) fn rename_if(
        &mut self,
        r: &ConditionalRename,
        gc: &mut Vec<ExtentId>,
    ) -> Result<(), MetaError> {
        check_name(&r.new_name)?;
        let f = self.filesystem(&r.fs)?;
        let dst = f.entry(r.new_parent, &r.new_name)?;
        if f.entry(r.parent, &r.name)? != Some(r.ino) {
            if dst == Some(r.ino) {
                return Ok(());
            }
            return Err(MetaError::NotFound(format!(
                "inode {} as {:?} in directory {}",
                r.ino, r.name, r.parent
            )));
        }
        let holds = match (&r.expect, dst) {
            (None, _) | (Some(RenameExpect::Absent), None) => true,
            (
                Some(RenameExpect::Unchanged {
                    ino,
                    size,
                    mtime_ns,
                    xattr,
                    value,
                }),
                Some(d),
            ) if d == *ino => {
                let i = f.inode(d)?;
                i.size() == *size
                    && i.mtime_ns == *mtime_ns
                    && i.xattrs.get(xattr) == value.as_ref()
            }
            _ => false,
        };
        if !holds {
            return Err(MetaError::Precondition(format!(
                "{:?} in directory {} is not as expected",
                r.new_name, r.new_parent
            )));
        }
        if let (Some(xattr), Some(d)) = (&r.retain_xattr, dst) {
            let until = retained_until(&*f.inode(d)?, xattr);
            if d != r.ino && i64::try_from(until).unwrap_or(i64::MAX) > r.now_ns / 1_000_000_000 {
                return Err(MetaError::Locked(format!(
                    "{:?} in directory {} is retention-locked until {until}",
                    r.new_name, r.new_parent
                )));
            }
        }
        self.apply_fs(
            &FsOp::Rename {
                fs: r.fs.clone(),
                parent: r.parent,
                name: r.name.clone(),
                new_parent: r.new_parent,
                new_name: r.new_name.clone(),
                now_ns: r.now_ns,
            },
            gc,
        )?;
        // The log now holds a command only builds with this capability decode, so a later
        // leader may propose another without first hearing every replica advertise it.
        self.features |= CAP_RENAME_IF;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        metadata::MetaCommand,
        namespace::{NodeType, ReplicaState, XattrMode, ROOT_INO},
    };

    const META: &str = "user.meta";
    const RETAIN: &str = "user.retain";
    const SEC: i64 = 1_000_000_000;

    struct T {
        c: Catalog,
        index: u64,
    }

    impl T {
        fn new() -> Self {
            let mut t = Self {
                c: Catalog::default(),
                index: 0,
            };
            t.ok(FsOp::CreateFs {
                fs: "f".into(),
                name: "fs".into(),
                now_ns: 1,
                extent_bytes: None,
            });
            t
        }

        fn run(&mut self, op: FsOp) -> Result<Vec<ExtentId>, MetaError> {
            let r = self.c.apply(1, self.index + 1, &MetaCommand::Fs { op });
            if r.is_ok() {
                self.index += 1;
            }
            r
        }

        fn ok(&mut self, op: FsOp) {
            self.run(op).unwrap();
        }

        fn entry(&self, name: &str) -> Option<u64> {
            self.c
                .filesystem("f")
                .unwrap()
                .entry(ROOT_INO, name)
                .unwrap()
        }

        fn file(&mut self, name: &str) -> u64 {
            self.ok(FsOp::Mknode {
                fs: "f".into(),
                parent: ROOT_INO,
                name: name.into(),
                op_id: String::new(),
                node_type: NodeType::File,
                target: None,
                rdev: 0,
                mode: 0o644,
                create_mode: None,
                uid: 0,
                gid: 0,
                now_ns: 2,
            });
            self.entry(name).unwrap()
        }

        fn set(&mut self, ino: u64, name: &str, value: &str) {
            self.ok(FsOp::SetXattr {
                fs: "f".into(),
                ino,
                name: name.into(),
                value: value.into(),
                mode: XattrMode::Set,
                now_ns: 3,
            });
        }

        /// What a client reads of `name` before publishing over it.
        fn read(&self, name: &str) -> RenameExpect {
            let Some(ino) = self.entry(name) else {
                return RenameExpect::Absent;
            };
            let i = self.c.filesystem("f").unwrap().inode(ino).unwrap();
            RenameExpect::Unchanged {
                ino,
                size: i.size(),
                mtime_ns: i.mtime_ns,
                xattr: META.into(),
                value: i.xattrs.get(META).cloned(),
            }
        }
    }

    fn publish(name: &str, ino: u64, to: &str, expect: Option<RenameExpect>) -> ConditionalRename {
        ConditionalRename {
            fs: "f".into(),
            parent: ROOT_INO,
            name: name.into(),
            ino,
            new_parent: ROOT_INO,
            new_name: to.into(),
            expect,
            retain_xattr: Some(RETAIN.into()),
            now_ns: 100 * SEC,
        }
    }

    #[test]
    fn only_one_of_two_creates_wins() {
        let mut t = T::new();
        let (a, b) = (t.file("a"), t.file("b"));
        t.ok(FsOp::RenameIf(publish(
            "a",
            a,
            "k",
            Some(RenameExpect::Absent),
        )));
        let before = t.c.clone();
        let err = t
            .run(FsOp::RenameIf(publish(
                "b",
                b,
                "k",
                Some(RenameExpect::Absent),
            )))
            .unwrap_err();
        assert!(matches!(err, MetaError::Precondition(_)), "{err}");
        assert_eq!(
            serde_json::to_value(&t.c).unwrap(),
            serde_json::to_value(&before).unwrap(),
            "a refused rename changed the catalog"
        );
        assert_eq!((t.entry("k"), t.entry("b")), (Some(a), Some(b)));
        assert_eq!(t.c.features & CAP_RENAME_IF, CAP_RENAME_IF);
    }

    #[test]
    fn a_replacement_needs_the_destination_as_it_was_read() {
        let mut t = T::new();
        let old = t.file("k");
        t.set(old, META, "v1");
        let read = t.read("k");
        // Changed in place after it was read.
        t.set(old, META, "v2");
        let a = t.file("a");
        let err = t
            .run(FsOp::RenameIf(publish("a", a, "k", Some(read))))
            .unwrap_err();
        assert!(matches!(err, MetaError::Precondition(_)), "{err}");
        // An unrelated attribute doesn't matter.
        let read = t.read("k");
        t.set(old, "user.tags", "x");
        t.ok(FsOp::RenameIf(publish("a", a, "k", Some(read.clone()))));
        assert_eq!(t.entry("k"), Some(a));
        // Replaced by another file.
        let b = t.file("b");
        assert!(t
            .run(FsOp::RenameIf(publish("b", b, "k", Some(read))))
            .is_err());
        // Removed.
        let read = t.read("k");
        t.ok(FsOp::Unlink {
            fs: "f".into(),
            parent: ROOT_INO,
            name: "k".into(),
            now_ns: 4,
        });
        assert!(t
            .run(FsOp::RenameIf(publish("b", b, "k", Some(read))))
            .is_err());
        assert_eq!(t.entry("k"), None);
    }

    #[test]
    fn a_repeat_does_nothing_and_a_lost_source_is_not_found() {
        let mut t = T::new();
        let a = t.file("a");
        let op = publish("a", a, "k", Some(RenameExpect::Absent));
        t.ok(FsOp::RenameIf(op.clone()));
        t.ok(FsOp::RenameIf(op.clone()));
        assert_eq!(t.entry("k"), Some(a));
        // Once something else replaced it, the repeat cannot tell: not found.
        let b = t.file("b");
        t.ok(FsOp::RenameIf(publish("b", b, "k", None)));
        assert!(matches!(
            t.run(FsOp::RenameIf(op)),
            Err(MetaError::NotFound(_))
        ));
        assert_eq!(t.entry("k"), Some(b));
        // The source name now names another inode.
        let c = t.file("c");
        assert!(t
            .run(FsOp::RenameIf(publish("c", c + 100, "k", None)))
            .is_err());
    }

    #[test]
    fn a_retained_destination_is_not_replaced() {
        let mut t = T::new();
        let old = t.file("k");
        t.set(old, RETAIN, "101");
        let a = t.file("a");
        for expect in [None, Some(t.read("k"))] {
            let err = t
                .run(FsOp::RenameIf(publish("a", a, "k", expect)))
                .unwrap_err();
            assert!(matches!(err, MetaError::Locked(_)), "{err}");
        }
        // A failed condition is reported ahead of the lock.
        assert!(matches!(
            t.run(FsOp::RenameIf(publish(
                "a",
                a,
                "k",
                Some(RenameExpect::Absent)
            ))),
            Err(MetaError::Precondition(_))
        ));
        let mut later = publish("a", a, "k", None);
        later.now_ns = 101 * SEC;
        t.ok(FsOp::RenameIf(later));
        assert_eq!(t.entry("k"), Some(a));
    }

    #[test]
    fn replicas_refuse_it_and_it_decodes() {
        let mut t = T::new();
        let a = t.file("a");
        let op = FsOp::RenameIf(publish("a", a, "k", Some(t.read("k"))));
        let text = serde_json::to_string(&op).unwrap();
        assert!(text.contains(r#""op":"rename_if""#), "{text}");
        assert_eq!(serde_json::from_str::<FsOp>(&text).unwrap(), op);
        t.c.fs_mut("f").unwrap().replica = Some(ReplicaState::default());
        assert!(matches!(t.run(op), Err(MetaError::ReadOnly(_))));
    }
}
