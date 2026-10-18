// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Dataset locality: where the extents of a file or directory tree live, and moving one copy of
//! each onto chosen hosts so a job scheduled there reads its dataset from local data nodes.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    ops::Bound,
};

use serde::{Deserialize, Serialize};

use super::{NativeEngine, NativeError};
use crate::{
    metadata::{ExtentId, MetaCommand},
    namespace::{InodeKind, ROOT_INO},
    raft::RaftError,
};

/// Directory entries read per page while walking a tree.
const WALK_PAGE: usize = 1024;

/// Where a file or directory tree's data lives; see [`NativeEngine::fs_locality`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Locality {
    pub files: u64,
    pub dirs: u64,
    /// Distinct extents (an extent shared by hard links, clones or snapshots counts once).
    pub extents: u64,
    /// Logical bytes of those extents.
    pub bytes: u64,
    /// Bytes in extents tiered to object storage (no data node holds them).
    pub tiered_bytes: u64,
    /// Bytes in erasure-coded extents (every node holds a shard, none a full copy).
    pub erasure_coded_bytes: u64,
    /// Every data node, most bytes first.
    pub nodes: Vec<NodeLocality>,
    /// Every host, most bytes first.
    pub hosts: Vec<HostLocality>,
    /// The walk stopped after `max_inodes` inodes; the figures cover only what it reached.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeLocality {
    pub node_id: String,
    pub host: String,
    pub rack: String,
    pub zone: String,
    pub up: bool,
    /// Bytes of replicated extents with a full copy on this node.
    pub bytes: u64,
    /// Bytes of erasure-coded shards on this node.
    pub shard_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostLocality {
    pub host: String,
    pub zone: String,
    /// Bytes of replicated extents with a full copy on one of this host's nodes.
    pub bytes: u64,
    /// `bytes` over all replicated bytes: the share of the dataset a reader on this host gets
    /// from local data nodes.
    pub local_fraction: f64,
}

/// What one [`NativeEngine::fs_pin`] call did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinReport {
    /// Extents looked at in this call.
    pub examined: u64,
    /// Already had a full copy on a target host.
    pub local: u64,
    /// Got a copy on a target host (one replica moved there).
    pub moved: u64,
    pub bytes_moved: u64,
    /// Skipped: tiered to object storage.
    pub tiered: u64,
    /// Skipped: erasure-coded (a shard on a host is not a local copy).
    pub erasure_coded: u64,
    /// Left for a later call: no readable copy, no eligible node on the target hosts, or the
    /// extent changed meanwhile.
    pub deferred: u64,
    /// Pass as `after` to continue; absent once the tree's last extent was examined.
    pub next: Option<String>,
    pub truncated: bool,
}

#[derive(Default)]
struct Walk {
    files: u64,
    dirs: u64,
    extents: BTreeSet<ExtentId>,
    truncated: bool,
}

impl NativeEngine {
    /// The inode at `path` (`/`-separated from the filesystem root; symlinks are not followed).
    pub fn fs_resolve(&self, fs: &str, path: &str) -> Result<u64, NativeError> {
        self.with_fs(fs, |_, f| {
            let mut ino = ROOT_INO;
            for name in path.split('/').filter(|n| !n.is_empty() && *n != ".") {
                if name == ".." {
                    return Err(NativeError::Invalid(format!(
                        "path {path:?}: \"..\" is not allowed"
                    )));
                }
                ino = f.lookup(ino, name)?;
            }
            Ok(ino)
        })
    }

    /// The extents of `root` and, for a directory, of everything below it, visiting at most
    /// `max_inodes` inodes.
    fn walk(&self, fs: &str, root: u64, max_inodes: usize) -> Result<Walk, NativeError> {
        self.with_fs(fs, |_, f| {
            let mut w = Walk::default();
            let mut seen = HashSet::new();
            let mut stack = vec![root];
            while let Some(ino) = stack.pop() {
                if seen.contains(&ino) {
                    continue;
                }
                if seen.len() >= max_inodes {
                    w.truncated = true;
                    break;
                }
                seen.insert(ino);
                let inode = f.inode(ino)?;
                match &inode.kind {
                    InodeKind::File { extents, .. } => {
                        w.files += 1;
                        w.extents.extend(extents.values().cloned());
                    }
                    InodeKind::Dir { .. } => {
                        w.dirs += 1;
                        let mut after: Option<String> = None;
                        loop {
                            let page = f.entries(ino, after.as_deref(), WALK_PAGE)?;
                            let full = page.len() == WALK_PAGE;
                            after = page.last().map(|(name, _)| name.clone());
                            stack.extend(page.into_iter().map(|(_, child)| child));
                            if !full {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(w)
        })
    }

    /// Bytes of the tree at `path` per data node and per host.
    pub fn fs_locality(
        &self,
        fs: &str,
        path: &str,
        max_inodes: usize,
    ) -> Result<Locality, NativeError> {
        let root = self.fs_resolve(fs, path)?;
        let w = self.walk(fs, root, max_inodes)?;
        let host_of: BTreeMap<&str, &str> = self
            .nodes
            .iter()
            .map(|n| (n.spec.id.as_str(), n.spec.failure_domain.host.as_str()))
            .collect();
        let mut per_node: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
        let mut per_host: BTreeMap<&str, u64> = BTreeMap::new();
        let (mut extents, mut bytes, mut tiered, mut ec) = (0u64, 0u64, 0u64, 0u64);
        self.with_catalog(|c| {
            for id in &w.extents {
                let Some(m) = c.extents.get(id) else {
                    continue;
                };
                let ext = &m.extent;
                let len = ext.len() as u64;
                extents += 1;
                bytes += len;
                if ext.object.is_some() {
                    tiered += len;
                    continue;
                }
                if ext.ec.is_some() {
                    ec += len;
                    for r in &ext.replicas {
                        if let Some((id, _)) = host_of.get_key_value(r.node_id.as_str()) {
                            per_node.entry(id).or_default().1 += ext.stored_len();
                        }
                    }
                    continue;
                }
                let mut hosts = BTreeSet::new();
                for r in &ext.replicas {
                    if let Some((id, host)) = host_of.get_key_value(r.node_id.as_str()) {
                        per_node.entry(id).or_default().0 += len;
                        hosts.insert(*host);
                    }
                }
                for h in hosts {
                    *per_host.entry(h).or_default() += len;
                }
            }
        })?;
        let replicated = bytes - tiered - ec;
        let mut nodes: Vec<NodeLocality> = self
            .nodes
            .iter()
            .map(|n| {
                let (b, s) = per_node
                    .get(n.spec.id.as_str())
                    .copied()
                    .unwrap_or_default();
                NodeLocality {
                    node_id: n.spec.id.clone(),
                    host: n.spec.failure_domain.host.clone(),
                    rack: n.spec.failure_domain.rack.clone(),
                    zone: n.spec.failure_domain.zone.clone(),
                    up: n.spec.healthy && self.is_up(n),
                    bytes: b,
                    shard_bytes: s,
                }
            })
            .collect();
        nodes.sort_by(|a, b| {
            (b.bytes + b.shard_bytes)
                .cmp(&(a.bytes + a.shard_bytes))
                .then_with(|| a.node_id.cmp(&b.node_id))
        });
        let mut hosts: Vec<HostLocality> = Vec::new();
        for n in &self.nodes {
            let fd = &n.spec.failure_domain;
            if hosts.iter().any(|h| h.host == fd.host) {
                continue;
            }
            let b = per_host.get(fd.host.as_str()).copied().unwrap_or_default();
            hosts.push(HostLocality {
                host: fd.host.clone(),
                zone: fd.zone.clone(),
                bytes: b,
                local_fraction: match replicated {
                    0 => 0.0,
                    r => b as f64 / r as f64,
                },
            });
        }
        hosts.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.host.cmp(&b.host)));
        Ok(Locality {
            files: w.files,
            dirs: w.dirs,
            extents,
            bytes,
            tiered_bytes: tiered,
            erasure_coded_bytes: ec,
            nodes,
            hosts,
            truncated: w.truncated,
        })
    }

    /// Gives every replicated extent of the tree at `path` a full copy on one of `hosts`, by
    /// moving one of its replicas there (the replica count stays the same). Examines at most
    /// `max_extents` extents in id order after `after` per call; repeat with
    /// [`PinReport::next`] until it is absent. Later writes are placed as usual, and repair may
    /// move a pinned copy off a host that fails, so pinning again after either is harmless.
    pub fn fs_pin(
        &self,
        fs: &str,
        path: &str,
        hosts: &[String],
        after: Option<&str>,
        max_extents: usize,
        max_inodes: usize,
    ) -> Result<PinReport, NativeError> {
        let targets: BTreeSet<&str> = hosts.iter().map(String::as_str).collect();
        if targets.is_empty() {
            return Err(NativeError::Invalid("at least one host is required".into()));
        }
        for h in &targets {
            if !self.nodes.iter().any(|n| n.spec.failure_domain.host == *h) {
                return Err(NativeError::Invalid(format!("no data node on host {h:?}")));
            }
        }
        if max_extents == 0 {
            return Err(NativeError::Invalid("max_extents must be positive".into()));
        }
        let root = self.fs_resolve(fs, path)?;
        let w = self.walk(fs, root, max_inodes)?;
        let fence = self.write_fence()?;
        let start = after.map_or(Bound::Unbounded, Bound::Excluded);
        let batch: Vec<&ExtentId> = w
            .extents
            .range::<str, _>((start, Bound::Unbounded))
            .take(max_extents)
            .collect();
        let mut report = PinReport {
            truncated: w.truncated,
            ..PinReport::default()
        };
        for id in &batch {
            report.examined += 1;
            self.pin_extent(id, &targets, fence, &mut report)?;
        }
        if batch.len() == max_extents {
            report.next = batch.last().map(|id| id.to_string());
        }
        Ok(report)
    }

    fn pin_extent(
        &self,
        id: &str,
        targets: &BTreeSet<&str>,
        fence: u64,
        report: &mut PinReport,
    ) -> Result<(), NativeError> {
        let Some(ext) = self.with_catalog(|c| c.extents.get(id).map(|e| e.extent.clone()))? else {
            return Ok(());
        };
        if ext.object.is_some() {
            report.tiered += 1;
            return Ok(());
        }
        if ext.ec.is_some() {
            report.erasure_coded += 1;
            return Ok(());
        }
        let domain = |node_id: &str| self.node(node_id).ok().map(|n| &n.spec.failure_domain);
        if ext
            .replicas
            .iter()
            .any(|r| domain(&r.node_id).is_some_and(|d| targets.contains(d.host.as_str())))
        {
            report.local += 1;
            return Ok(());
        }
        let Ok(data) = self.read_placed(&ext) else {
            report.deferred += 1;
            return Ok(());
        };
        let _write = self
            .write_lock
            .lock()
            .map_err(|_| NativeError::Poisoned("write"))?;
        let unchanged = self.with_catalog(|c| {
            c.extents
                .get(&ext.id)
                .is_some_and(|e| e.extent.replicas == ext.replicas)
        })?;
        if !unchanged {
            report.deferred += 1;
            return Ok(());
        }
        // No replica is on a target host, so the new copy never shares a host with the others.
        let taken: BTreeSet<&str> = ext.replicas.iter().map(|r| r.node_id.as_str()).collect();
        let order = self.placement_order(data.len() as u64, |n| {
            !targets.contains(n.failure_domain.host.as_str()) || taken.contains(n.id.as_str())
        });
        let alloc = self.lease()?;
        let mut placed = None;
        for node_id in &order {
            if let Some(r) = self.place_replica(node_id, fence, &data, &alloc)? {
                placed = Some(r);
                break;
            }
        }
        let Some(new) = placed else {
            report.deferred += 1;
            return Ok(());
        };
        // Give up a copy on the new node's rack when there is one, so rack spread doesn't shrink.
        let rack = domain(&new.node_id).map(|d| d.rack.clone());
        let Some(old) = ext
            .replicas
            .iter()
            .find(|r| domain(&r.node_id).map(|d| &d.rack) == rack.as_ref())
            .or(ext.replicas.last())
            .cloned()
        else {
            report.deferred += 1;
            return Ok(());
        };
        let cmd = MetaCommand::ReplaceReplica {
            extent_id: ext.id.clone(),
            old,
            new,
        };
        match self
            .commit(cmd, Some(fence))
            .inspect_err(|e| alloc.settle(e))
        {
            Ok(()) => {
                report.moved += 1;
                report.bytes_moved += data.len() as u64;
                Ok(())
            }
            Err(NativeError::Metadata(_) | NativeError::Raft(RaftError::Rejected(_))) => {
                report.deferred += 1;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}
