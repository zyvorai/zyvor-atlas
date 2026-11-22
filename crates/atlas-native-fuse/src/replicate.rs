// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Asynchronous filesystem replication between two native clusters (binary
//! `atlas-native-replicate`). Each round snapshots the source filesystem, sends the changes
//! since the last replicated snapshot to a replica filesystem on the target and commits them
//! there as a snapshot of its own. The replicator keeps no state: the replica's `base` and
//! `pending` snapshots say where to continue, so a round cut short resumes after a restart.

use std::{
    collections::BTreeSet,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use atlas_native::{
    engine::{DiffInode, DiffPage},
    namespace::{ReplicaInode, ReplicaState},
};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::client::{encode, Body, Client, Error, Retry};

#[derive(Debug, Clone)]
pub struct ReplicateConfig {
    /// Source filesystem id.
    pub fs: String,
    /// Replica filesystem id on the target.
    pub target_fs: String,
    /// Id prefix of the snapshots the replicator takes (they are its to delete).
    pub prefix: String,
    /// Replicated snapshots kept on the target (at least the latest is always kept).
    pub keep: usize,
    /// Inodes per diff page.
    pub diff_limit: usize,
    /// Rough upper bound of one metadata request body.
    pub batch_bytes: usize,
    /// Largest data request; keep at or below both clusters' `max_request_bytes`.
    pub max_io_bytes: usize,
    /// Files copied at once.
    pub parallel: usize,
}

impl ReplicateConfig {
    pub fn new(fs: impl Into<String>, target_fs: impl Into<String>) -> Self {
        Self {
            fs: fs.into(),
            target_fs: target_fs.into(),
            prefix: "repl-".into(),
            keep: 24,
            diff_limit: 512,
            batch_bytes: 512 << 10,
            max_io_bytes: 4 << 20,
            parallel: 4,
        }
    }
}

/// What one round sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Round {
    /// The source snapshot the replica now matches.
    pub snapshot: String,
    /// `None` for the first, full round.
    pub from: Option<String>,
    /// Whether it continued an increment an earlier round left partly applied.
    pub resumed: bool,
    pub inodes: usize,
    pub removed: usize,
    pub bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ReplicateError {
    #[error("source: {0}")]
    Source(Error),
    #[error("target: {0}")]
    Target(Error),
    #[error("{0}")]
    State(String),
}

#[derive(Deserialize)]
struct FsEntry {
    id: String,
    name: String,
    extent_bytes: u64,
    #[serde(default)]
    replica: Option<ReplicaState>,
}

#[derive(Deserialize)]
struct SnapEntry {
    id: String,
    fs_id: String,
    created_ns: i64,
}

/// Directory entries per replica inode in one request: a larger change is split.
const ENTRIES_PER_PART: usize = 4096;

pub struct Replicator {
    source: Client,
    target: Client,
    cfg: ReplicateConfig,
}

impl Replicator {
    pub fn new(source: Client, target: Client, cfg: ReplicateConfig) -> Self {
        Self {
            source,
            target,
            cfg,
        }
    }

    pub fn config(&self) -> &ReplicateConfig {
        &self.cfg
    }

    /// Brings the replica up to a new snapshot of the source (or finishes a partial round).
    pub fn run_once(&self) -> Result<Round, ReplicateError> {
        let src = find_fs(&self.source, &self.cfg.fs)
            .map_err(ReplicateError::Source)?
            .ok_or_else(|| {
                ReplicateError::State(format!("source has no filesystem {}", self.cfg.fs))
            })?;
        if src.replica.is_some() {
            return Err(ReplicateError::State(format!(
                "source filesystem {} is itself a replica; promote it first",
                self.cfg.fs
            )));
        }
        let state = self.replica_state(&src)?;
        let src_snaps = snapshots(&self.source, &self.cfg.fs).map_err(ReplicateError::Source)?;
        let has = |id: &str| src_snaps.iter().any(|s| s.id == id);
        if let Some(base) = &state.base {
            if !has(base) {
                return Err(ReplicateError::State(format!(
                    "the replica's base snapshot {base} is gone from the source; seed a new replica"
                )));
            }
        }
        let (to, resumed) = match &state.pending {
            Some(p) if has(p) => (p.clone(), true),
            _ => (self.take_snapshot()?, false),
        };
        let from = state.base.clone();
        let mut round = Round {
            snapshot: to.clone(),
            from: from.clone(),
            resumed,
            ..Round::default()
        };
        let mut after = 0;
        let mut next_ino = 0;
        loop {
            let page = self.diff(&to, from.as_deref(), after)?;
            next_ino = next_ino.max(page.next_ino);
            round.inodes += page.inodes.len();
            round.removed += page.removed.len();
            self.apply(&from, &to, &page)?;
            round.bytes += self.copy_data(&to, &page.inodes)?;
            match page.next {
                Some(n) => after = n,
                None => break,
            }
        }
        self.target
            .request(
                Method::POST,
                &format!("/v1/fs/{}/replica/commit", self.cfg.target_fs),
                Body::Json(json!({ "from": from, "to": to, "snapshot": to, "next_ino": next_ino })),
                Retry::Idempotent,
            )
            .map_err(ReplicateError::Target)?;
        self.prune(&to)?;
        Ok(round)
    }

    /// The target replica's state, creating the replica on first use.
    fn replica_state(&self, src: &FsEntry) -> Result<ReplicaState, ReplicateError> {
        match find_fs(&self.target, &self.cfg.target_fs).map_err(ReplicateError::Target)? {
            Some(FsEntry {
                replica: Some(r),
                extent_bytes,
                ..
            }) => {
                if extent_bytes != src.extent_bytes {
                    return Err(ReplicateError::State(format!(
                        "replica {} has a {extent_bytes}-byte grid, the source {}",
                        self.cfg.target_fs, src.extent_bytes
                    )));
                }
                Ok(r)
            }
            Some(_) => Err(ReplicateError::State(self.writable_target_hint())),
            None => {
                self.target
                    .request(
                        Method::POST,
                        "/v1/fs",
                        Body::Json(json!({
                            "id": self.cfg.target_fs,
                            "name": src.name,
                            "replica": true,
                            "extent_bytes": src.extent_bytes,
                        })),
                        Retry::Idempotent,
                    )
                    .map_err(ReplicateError::Target)?;
                tracing::info!(replica = %self.cfg.target_fs, "created the replica filesystem");
                Ok(ReplicaState::default())
            }
        }
    }

    /// Why a writable target is refused, naming the newest snapshot both sides share so the
    /// target can be demoted onto it (failback).
    fn writable_target_hint(&self) -> String {
        let mut msg = format!(
            "target filesystem {} is writable, not a replica",
            self.cfg.target_fs
        );
        let (Ok(src), Ok(dst)) = (
            snapshots(&self.source, &self.cfg.fs),
            snapshots(&self.target, &self.cfg.target_fs),
        ) else {
            return msg;
        };
        let theirs: BTreeSet<&str> = src.iter().map(|s| s.id.as_str()).collect();
        if let Some(common) = dst
            .iter()
            .filter(|s| theirs.contains(s.id.as_str()))
            .max_by_key(|s| s.created_ns)
        {
            msg.push_str(&format!(
                "; to fail back, demote it onto the snapshot both sides have: POST \
                 /v1/fs/{}/demote {{\"snapshot\":\"{id}\",\"base\":\"{id}\"}}",
                self.cfg.target_fs,
                id = common.id,
            ));
        }
        msg
    }

    fn take_snapshot(&self) -> Result<String, ReplicateError> {
        let ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let id = format!("{}{ns}", self.cfg.prefix);
        self.source
            .request(
                Method::POST,
                &format!("/v1/fs/{}/snapshots", self.cfg.fs),
                Body::Json(json!({ "id": id, "name": id })),
                Retry::Idempotent,
            )
            .map_err(ReplicateError::Source)?;
        Ok(id)
    }

    fn diff(&self, to: &str, from: Option<&str>, after: u64) -> Result<DiffPage, ReplicateError> {
        let mut path = format!(
            "/v1/fs-snapshots/{to}/diff?after={after}&limit={}",
            self.cfg.diff_limit
        );
        if let Some(f) = from {
            path.push_str(&format!("&from={}", encode(f)));
        }
        let b = self
            .source
            .request(Method::GET, &path, Body::Empty, Retry::Idempotent)
            .map_err(ReplicateError::Source)?;
        serde_json::from_slice(&b)
            .map_err(|e| ReplicateError::Source(Error::Decode(format!("diff of {to}: {e}"))))
    }

    /// Sends one diff page's metadata in parts of at most about `batch_bytes`.
    fn apply(
        &self,
        from: &Option<String>,
        to: &str,
        page: &DiffPage,
    ) -> Result<(), ReplicateError> {
        let mut part: Vec<ReplicaInode> = Vec::new();
        let mut size = 0;
        for d in &page.inodes {
            for piece in split_entries(&d.inode) {
                let n = serde_json::to_vec(&piece).map_or(0, |v| v.len());
                if !part.is_empty() && size + n > self.cfg.batch_bytes {
                    self.send_part(from, to, std::mem::take(&mut part), &[])?;
                    size = 0;
                }
                size += n;
                part.push(piece);
            }
        }
        if !part.is_empty() || !page.removed.is_empty() {
            self.send_part(from, to, part, &page.removed)?;
        }
        Ok(())
    }

    fn send_part(
        &self,
        from: &Option<String>,
        to: &str,
        inodes: Vec<ReplicaInode>,
        removed: &[u64],
    ) -> Result<(), ReplicateError> {
        self.target
            .request(
                Method::POST,
                &format!("/v1/fs/{}/replica/apply", self.cfg.target_fs),
                Body::Json(json!({ "from": from, "to": to, "inodes": inodes, "removed": removed })),
                Retry::Idempotent,
            )
            .map(drop)
            .map_err(ReplicateError::Target)
    }

    /// Copies the changed file ranges from the source snapshot into the replica.
    fn copy_data(&self, to: &str, inodes: &[DiffInode]) -> Result<u64, ReplicateError> {
        let mut work: Vec<(u64, u64, u64)> = Vec::new();
        let chunk = self.cfg.max_io_bytes.max(4096) as u64;
        for d in inodes {
            for &(off, len) in &d.data {
                let mut o = off;
                while o < off + len {
                    let n = chunk.min(off + len - o);
                    work.push((d.inode.ino, o, n));
                    o += n;
                }
            }
        }
        if work.is_empty() {
            return Ok(0);
        }
        let queue = Mutex::new(work.into_iter());
        let copied = Mutex::new(0u64);
        let failed: Mutex<Option<ReplicateError>> = Mutex::new(None);
        std::thread::scope(|s| {
            for _ in 0..self.cfg.parallel.max(1) {
                s.spawn(|| loop {
                    if failed.lock().expect("lock").is_some() {
                        return;
                    }
                    let Some((ino, off, len)) = queue.lock().expect("lock").next() else {
                        return;
                    };
                    match self.copy_range(to, ino, off, len) {
                        Ok(n) => *copied.lock().expect("lock") += n,
                        Err(e) => {
                            failed.lock().expect("lock").get_or_insert(e);
                            return;
                        }
                    }
                });
            }
        });
        match failed.into_inner().expect("lock") {
            Some(e) => Err(e),
            None => Ok(copied.into_inner().expect("lock")),
        }
    }

    fn copy_range(&self, to: &str, ino: u64, off: u64, len: u64) -> Result<u64, ReplicateError> {
        let data = self
            .source
            .request(
                Method::GET,
                &format!(
                    "/v1/fs/{}@{to}/inodes/{ino}/data?offset={off}&len={len}",
                    self.cfg.fs
                ),
                Body::Empty,
                Retry::Idempotent,
            )
            .map_err(ReplicateError::Source)?;
        if data.is_empty() {
            return Ok(0);
        }
        self.target
            .request(
                Method::PUT,
                &format!(
                    "/v1/fs/{}/replica/inodes/{ino}/data?offset={off}",
                    self.cfg.target_fs
                ),
                Body::Bytes(&data),
                Retry::Idempotent,
            )
            .map_err(ReplicateError::Target)?;
        Ok(data.len() as u64)
    }

    /// Deletes the replicator's source snapshots other than the new base, and all but the
    /// newest `keep` on the target. A failed deletion is logged, not fatal: the next round
    /// tries again.
    fn prune(&self, base: &str) -> Result<(), ReplicateError> {
        let ours = |s: &SnapEntry| s.id.starts_with(&self.cfg.prefix);
        match snapshots(&self.source, &self.cfg.fs) {
            Ok(src) => {
                for s in src.iter().filter(|s| ours(s) && s.id != base) {
                    delete_snapshot(&self.source, &s.id, "source");
                }
            }
            Err(e) => tracing::warn!(error = %e, "listing source snapshots to prune"),
        }
        match snapshots(&self.target, &self.cfg.target_fs) {
            Ok(mut dst) => {
                dst.retain(|s| ours(s) && s.id != base);
                dst.sort_by_key(|s| std::cmp::Reverse(s.created_ns));
                for s in dst.iter().skip(self.cfg.keep.saturating_sub(1)) {
                    delete_snapshot(&self.target, &s.id, "target");
                }
            }
            Err(e) => tracing::warn!(error = %e, "listing target snapshots to prune"),
        }
        Ok(())
    }
}

/// `inode` as one or more copies that each set at most [`ENTRIES_PER_PART`] directory entries.
fn split_entries(inode: &ReplicaInode) -> Vec<ReplicaInode> {
    if inode.entries.len() <= ENTRIES_PER_PART {
        return vec![inode.clone()];
    }
    inode
        .entries
        .chunks(ENTRIES_PER_PART)
        .map(|c| ReplicaInode {
            entries: c.to_vec(),
            ..inode.clone()
        })
        .collect()
}

fn find_fs(c: &Client, id: &str) -> Result<Option<FsEntry>, Error> {
    let v = c.json(Method::GET, "/v1/fs", Body::Empty, Retry::Idempotent)?;
    let all: Vec<FsEntry> = serde_json::from_value(v["filesystems"].clone())
        .map_err(|e| Error::Decode(format!("/v1/fs: {e}")))?;
    Ok(all.into_iter().find(|f| f.id == id))
}

fn snapshots(c: &Client, fs: &str) -> Result<Vec<SnapEntry>, Error> {
    let v: Value = c.json(
        Method::GET,
        "/v1/fs-snapshots",
        Body::Empty,
        Retry::Idempotent,
    )?;
    let all: Vec<SnapEntry> = serde_json::from_value(v["snapshots"].clone())
        .map_err(|e| Error::Decode(format!("/v1/fs-snapshots: {e}")))?;
    Ok(all.into_iter().filter(|s| s.fs_id == fs).collect())
}

fn delete_snapshot(c: &Client, id: &str, side: &str) {
    match c.request(
        Method::DELETE,
        &format!("/v1/fs-snapshots/{id}"),
        Body::Empty,
        Retry::Remove,
    ) {
        Ok(_) => tracing::debug!(snapshot = id, side, "pruned snapshot"),
        Err(e) if e.is_not_found() => {}
        Err(e) => tracing::warn!(snapshot = id, side, error = %e, "pruning snapshot"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atlas_native::NodeType;

    #[test]
    fn large_entry_changes_are_split() {
        let mut i = ReplicaInode {
            ino: 1,
            node_type: NodeType::Dir,
            mode: 0o755,
            uid: 0,
            gid: 0,
            nlink: 2,
            atime_ns: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            xattrs: Default::default(),
            size: 0,
            holes: Vec::new(),
            parent: 1,
            entries: Vec::new(),
            target: None,
            rdev: 0,
        };
        assert_eq!(split_entries(&i).len(), 1);
        i.entries = (0..ENTRIES_PER_PART * 2 + 1)
            .map(|n| (format!("f{n}"), Some(n as u64 + 2)))
            .collect();
        let parts = split_entries(&i);
        assert_eq!(parts.len(), 3);
        assert_eq!(
            parts.iter().map(|p| p.entries.len()).sum::<usize>(),
            i.entries.len()
        );
        assert!(parts.iter().all(|p| p.ino == 1 && p.parent == 1));
    }
}
