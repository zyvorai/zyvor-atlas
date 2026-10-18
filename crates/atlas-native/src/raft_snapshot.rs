// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Snapshot transfer in chunks. The leader streams its applied catalog to a follower that fell
//! behind its compacted log: a header (everything but extents and the inodes of filesystems),
//! then pages of extents and of each filesystem's inodes, then `Done`. Each chunk waits for the
//! follower's acknowledgement of the previous one, so neither side holds more than a chunk in
//! flight.
//!
//! The leader reads a clone of its catalog, which keeps reading the store as of the clone (see
//! [`crate::inodes`]). The follower writes every chunk into a staging store next to its own and
//! swaps it in only once `Done` arrives, so a transfer cut short leaves the follower as it was.

use std::{
    fs, io,
    ops::Bound,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    inodes::InodeTable,
    metadata::{Catalog, ExtentId, ExtentMeta, MetaError},
    namespace::{FsId, Inode},
    store::CatalogStore,
};

const CHUNK_INODES: usize = 1024;
const CHUNK_EXTENTS: usize = 4096;
const STAGING: &str = "catalog.redb.incoming";

/// Externally tagged: an internally tagged enum buffers its content, and buffered integer map
/// keys don't decode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotChunk {
    /// The catalog without its extents and without the inodes of its filesystems.
    Header {
        catalog: Box<Catalog>,
    },
    Extents {
        extents: Vec<(ExtentId, ExtentMeta)>,
    },
    Inodes {
        fs: FsId,
        inodes: Vec<Inode>,
    },
    Done,
}

/// What follows the chunk in flight.
#[derive(Debug)]
enum Cursor {
    ExtentsAfter(Option<ExtentId>),
    Inodes { fs: usize, from: u64 },
    Done,
    End,
}

/// A transfer from the leader to one follower.
#[derive(Debug)]
pub(crate) struct Outgoing {
    /// The snapshot's last included index.
    pub index: u64,
    /// The chunk in flight.
    pub seq: u64,
    /// Tick the chunk in flight was (last) sent.
    pub sent_at: u64,
    pub chunk: SnapshotChunk,
    catalog: Catalog,
    filesystems: Vec<FsId>,
    next: Cursor,
}

impl Outgoing {
    pub fn start(catalog: Catalog, now: u64) -> Self {
        let header = Catalog {
            volumes: catalog.volumes.clone(),
            snapshots: catalog.snapshots.clone(),
            extents: Default::default(),
            applied_index: catalog.applied_index,
            current_term: catalog.current_term,
            free: catalog.free.clone(),
            membership: catalog.membership.clone(),
            raft_addrs: catalog.raft_addrs.clone(),
            filesystems: catalog
                .filesystems
                .iter()
                .map(|(id, f)| {
                    let mut f = f.clone();
                    f.inodes = InodeTable::default();
                    (id.clone(), f)
                })
                .collect(),
            fs_snapshots: catalog.fs_snapshots.clone(),
            leases: catalog.leases.clone(),
            in_store: false,
        };
        Self {
            index: catalog.applied_index,
            seq: 0,
            sent_at: now,
            chunk: SnapshotChunk::Header {
                catalog: Box::new(header),
            },
            filesystems: catalog.filesystems.keys().cloned().collect(),
            catalog,
            next: Cursor::ExtentsAfter(None),
        }
    }

    pub fn is_done(&self) -> bool {
        matches!(self.chunk, SnapshotChunk::Done)
    }

    /// Moves on to the next chunk.
    pub fn advance(&mut self, now: u64) -> Result<(), MetaError> {
        loop {
            let (chunk, next) = match &self.next {
                Cursor::ExtentsAfter(after) => {
                    let lower = match after {
                        Some(id) => Bound::Excluded(id),
                        None => Bound::Unbounded,
                    };
                    let page: Vec<(ExtentId, ExtentMeta)> = self
                        .catalog
                        .extents
                        .range::<ExtentId, _>((lower, Bound::Unbounded))
                        .take(CHUNK_EXTENTS)
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    if page.is_empty() {
                        self.next = Cursor::Inodes { fs: 0, from: 0 };
                        continue;
                    }
                    let last = page.last().map(|(k, _)| k.clone());
                    (
                        SnapshotChunk::Extents { extents: page },
                        Cursor::ExtentsAfter(last),
                    )
                }
                Cursor::Inodes { fs, from } => {
                    let Some(id) = self.filesystems.get(*fs) else {
                        self.next = Cursor::Done;
                        continue;
                    };
                    let page = match self.catalog.filesystems.get(id) {
                        Some(f) => f.inodes.page_full(*from, CHUNK_INODES)?,
                        None => Vec::new(),
                    };
                    let next = match page.last() {
                        Some(last) if page.len() == CHUNK_INODES => Cursor::Inodes {
                            fs: *fs,
                            from: last.ino + 1,
                        },
                        _ => Cursor::Inodes {
                            fs: fs + 1,
                            from: 0,
                        },
                    };
                    if page.is_empty() {
                        self.next = next;
                        continue;
                    }
                    (
                        SnapshotChunk::Inodes {
                            fs: id.clone(),
                            inodes: page
                                .into_iter()
                                .map(std::sync::Arc::unwrap_or_clone)
                                .collect(),
                        },
                        next,
                    )
                }
                Cursor::Done => (SnapshotChunk::Done, Cursor::End),
                Cursor::End => return Ok(()),
            };
            self.chunk = chunk;
            self.next = next;
            self.seq += 1;
            self.sent_at = now;
            return Ok(());
        }
    }
}

/// A transfer a follower is staging.
pub(crate) struct Incoming {
    pub index: u64,
    /// The last chunk staged.
    pub seq: u64,
    store: CatalogStore,
    catalog: Catalog,
}

impl std::fmt::Debug for Incoming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Incoming")
            .field("index", &self.index)
            .field("seq", &self.seq)
            .finish()
    }
}

pub(crate) fn staging_path(root: &Path) -> PathBuf {
    root.join(STAGING)
}

/// Drops a staging store left by a transfer that never finished.
pub(crate) fn remove_staging(root: &Path) -> io::Result<()> {
    match fs::remove_file(staging_path(root)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

impl Incoming {
    pub fn start(root: &Path, index: u64, mut header: Catalog) -> io::Result<Self> {
        remove_staging(root)?;
        let store = CatalogStore::open(staging_path(root))?;
        header.in_store = false;
        store.checkpoint(&mut header)?;
        Ok(Self {
            index,
            seq: 0,
            store,
            catalog: header,
        })
    }

    pub fn stage(&mut self, chunk: SnapshotChunk) -> io::Result<()> {
        match chunk {
            SnapshotChunk::Extents { extents } => {
                for (id, e) in extents {
                    self.catalog.extents.insert(id, e);
                }
            }
            SnapshotChunk::Inodes { fs, inodes } => {
                let f = self.catalog.filesystems.get_mut(&fs).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("snapshot inodes for unknown filesystem {fs}"),
                    )
                })?;
                for i in inodes {
                    f.inodes.insert(i);
                }
            }
            SnapshotChunk::Header { .. } | SnapshotChunk::Done => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "only extents and inodes are staged",
                ))
            }
        }
        self.store.checkpoint(&mut self.catalog)?;
        Ok(())
    }

    /// Closes the staging store (durable since its last checkpoint) and returns its path.
    pub fn finish(self) -> PathBuf {
        let path = self.store.path().to_path_buf();
        drop(self.catalog);
        drop(self.store);
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        metadata::{ExtentRef, MetaCommand, ReplicaRef},
        namespace::{FsOp, NodeType, ROOT_INO},
        raft::Message,
        store::{load_checkpoint, CATALOG_STORE},
    };

    #[test]
    fn a_catalog_streams_through_the_wire_into_a_staged_store() {
        let leader_dir = tempfile::tempdir().unwrap();
        let store = CatalogStore::open(leader_dir.path().join(CATALOG_STORE))
            .unwrap()
            .with_cache_inodes(64);
        let mut c = Catalog::default();
        let mut index = 0;
        let mut apply = |c: &mut Catalog, cmd: MetaCommand| {
            index += 1;
            c.apply(1, index, &cmd).unwrap();
        };
        apply(
            &mut c,
            MetaCommand::Fs {
                op: FsOp::CreateFs {
                    fs: "f".into(),
                    name: "f".into(),
                    now_ns: 1,
                    extent_bytes: None,
                },
            },
        );
        for i in 0..1300u64 {
            apply(
                &mut c,
                MetaCommand::Fs {
                    op: FsOp::Mknode {
                        fs: "f".into(),
                        parent: ROOT_INO,
                        name: format!("file-{i}"),
                        op_id: format!("op-{i}"),
                        node_type: NodeType::File,
                        target: None,
                        rdev: 0,
                        mode: 0o644,
                        create_mode: None,
                        uid: 0,
                        gid: 0,
                        now_ns: 2,
                    },
                },
            );
        }
        for i in 0..5000u64 {
            c.add_extent_ref(&ExtentRef {
                id: format!("e{i:05}"),
                logical_offset: 0,
                payload_len: 4096,
                raw_len: 0,
                checksum: [1; 32],
                content: None,
                replicas: vec![ReplicaRef {
                    node_id: "n1".into(),
                    device_index: 0,
                    offset: i * 4096,
                }],
                ec: None,
                created_ms: 0,
                object: None,
            });
        }
        // Paged on the leader's store, with some changes on top of it.
        store.checkpoint(&mut c).unwrap();
        apply(
            &mut c,
            MetaCommand::Fs {
                op: FsOp::Unlink {
                    fs: "f".into(),
                    parent: ROOT_INO,
                    name: "file-7".into(),
                    now_ns: 3,
                },
            },
        );

        let follower_dir = tempfile::tempdir().unwrap();
        let wire = |o: &Outgoing| -> SnapshotChunk {
            let msg = Message::InstallSnapshot {
                term: 1,
                index: o.index,
                seq: o.seq,
                chunk: Box::new(o.chunk.clone()),
            };
            let back: Message = serde_json::from_slice(&serde_json::to_vec(&msg).unwrap()).unwrap();
            let Message::InstallSnapshot { chunk, .. } = back else {
                panic!("not a snapshot chunk");
            };
            *chunk
        };
        let mut out = Outgoing::start(c.clone(), 0);
        let SnapshotChunk::Header { catalog } = wire(&out) else {
            panic!("a transfer starts with the header");
        };
        let mut inc = Incoming::start(follower_dir.path(), out.index, *catalog).unwrap();
        let mut chunks = 1;
        loop {
            out.advance(0).unwrap();
            chunks += 1;
            if out.is_done() {
                break;
            }
            inc.stage(wire(&out)).unwrap();
        }
        // Header, two extent pages, two inode pages and Done.
        assert_eq!(chunks, 6);
        let staged = inc.finish();
        let path = follower_dir.path().join(CATALOG_STORE);
        fs::rename(staged, &path).unwrap();
        let installed =
            load_checkpoint(follower_dir.path(), &CatalogStore::open(&path).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(&installed).unwrap(),
            serde_json::to_value(&c).unwrap()
        );
    }
}
