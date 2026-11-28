// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! A content tag for one file, for a client that keeps file data across opens
//! (`atlas-native-mount --keep-cache`). Two equal tags mean the bytes did not change in between,
//! including by way of a change that was later undone. It is computed from the applied catalog,
//! so every metadata node answers the same for the same state; nothing is stored for it.
//!
//! What it covers, and why:
//! - the extents and pending deltas the file points to (a write makes new ones);
//! - size and times (a write that reuses an existing extent, as deduplication does, still moves
//!   `ctime`);
//! - [`FsMeta::incarnation`], which moves whenever the whole tree is replaced (a filesystem
//!   created or cloned under a used id, a replica reverted), where extents and times come back;
//! - for a snapshot, when it was taken (its id can be used again).
//!
//! A fold of deltas changes the tag although the bytes stay: a client then merely re-reads.

use ring::digest::{Context, SHA256};

use super::{NativeEngine, NativeError};
use crate::namespace::{FsMeta, Inode, InodeKind};

/// Files with more extents and deltas than this get no tag: hashing them on every open would
/// hold the catalog for too long (64 GiB of 1 MiB cells).
pub const MAX_TAGGED_EXTENTS: usize = 1 << 16;

struct Hasher(Context);

impl Hasher {
    fn num(&mut self, n: u64) {
        self.0.update(&n.to_le_bytes());
    }

    /// Length first, so no two lists of fields run into the same bytes.
    fn text(&mut self, s: &str) {
        self.num(s.len() as u64);
        self.0.update(s.as_bytes());
    }
}

fn tag(f: &FsMeta, inode: &Inode, snapshot_taken_ns: i64) -> Option<String> {
    // A replica's data changes in place while an increment is applied, and can revert.
    if f.replica.is_some() {
        return None;
    }
    let InodeKind::File {
        size,
        extents,
        deltas,
    } = &inode.kind
    else {
        return None;
    };
    let pending: usize = deltas.values().map(Vec::len).sum();
    if extents.len().saturating_add(pending) > MAX_TAGGED_EXTENTS {
        return None;
    }
    let mut h = Hasher(Context::new(&SHA256));
    h.text("atlas-content-tag-v1");
    for n in [
        f.incarnation,
        snapshot_taken_ns as u64,
        inode.ino,
        *size,
        inode.mtime_ns as u64,
        inode.ctime_ns as u64,
        extents.len() as u64,
    ] {
        h.num(n);
    }
    for (at, id) in extents {
        h.num(*at);
        h.text(id);
    }
    h.num(deltas.len() as u64);
    for (cell, list) in deltas {
        h.num(*cell);
        h.num(list.len() as u64);
        for d in list {
            h.num(d.at);
            h.text(&d.id);
        }
    }
    let digest = h.0.finish();
    Some(digest.as_ref().iter().map(|b| format!("{b:02x}")).collect())
}

impl NativeEngine {
    /// The content tag of file `ino` of `fs` (or of `<fs>@<snapshot>`); `None` if a client must
    /// not keep the file's data (not a regular file, a replica filesystem, too many extents).
    /// Only as fresh as this replica's catalog: callers who need the latest run a read barrier
    /// first.
    pub fn fs_content_tag(&self, fs: &str, ino: u64) -> Result<Option<String>, NativeError> {
        self.with_catalog(|c| {
            let f = c.fs_view(fs)?;
            let inode = f.inode(ino)?;
            let taken = fs
                .split_once('@')
                .and_then(|(_, s)| c.fs_snapshots.get(s))
                .map_or(0, |s| s.created_ns);
            Ok::<_, NativeError>(tag(f, &inode, taken))
        })?
    }
}
