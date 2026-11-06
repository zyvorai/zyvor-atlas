// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Direct client writes: a client writes whole extents to the data nodes itself and the leader
//! only allocates and commits.
//!
//! 1. [`NativeEngine::direct_write_lease`]: the leader picks the replica targets for each
//!    extent-sized cell of a range, holds their free space in an [`inflight`] lease, and
//!    returns, per target, the data node's address, the device offset (or "append") and a
//!    [`Grant`]: an HMAC-tagged permission for exactly that write under the leader's term. One
//!    spare target per cell covers a replica that is down.
//! 2. The client writes the data to the targets (`RemoteDevice::write_at_granted` /
//!    `append_granted`). Data nodes refuse a write without a valid grant, an expired one, or
//!    one fenced by a later term.
//! 3. [`NativeEngine::direct_write_commit`]: the client reports which replicas succeeded and the
//!    extents' checksums; the leader checks them against the lease and commits the extents with
//!    the same `commit_cells` path a leader-side write uses (so quota, size, snapshots and Raft
//!    term fencing are unchanged).
//!
//! A lease nobody commits expires; its ranges then stay quarantined for a grace period (the
//! client may still be mid-write) before the free list offers them again. Leases are in leader
//! memory: after a failover the new leader does not know them, the commit is refused and the
//! client falls back to the leader path.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    files::writable, now_ms, Attr, ExtentRef, NativeEngine, NativeError, ReplicaRef, Target,
};
use crate::{
    grant::{from_hex, Grant},
    metadata::Catalog,
};

/// Most cells (extents) one lease covers.
pub const MAX_LEASE_CELLS: usize = 64;
/// Default lifetime of a lease, and of the grants in it.
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(30);
/// Default time an expired or aborted lease's ranges stay out of the free list.
pub const DEFAULT_EXPIRED_GRACE: Duration = Duration::from_secs(30);

/// Where one replica of a cell goes, and the permission to write it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectTarget {
    pub node_id: String,
    /// The data node's address.
    pub endpoint: String,
    /// The metadata group whose fence the write carries.
    pub group: u32,
    pub device_index: usize,
    /// Where to write; `None`: append, and report the offset the node returns with its receipt.
    pub offset: Option<u64>,
    pub grant: Grant,
}

/// One extent-sized cell: `needed` targets to write, then spares to use for ones that fail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectCell {
    pub logical_offset: u64,
    pub targets: Vec<DirectTarget>,
}

/// What `POST .../direct-write` returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectLease {
    pub lease: String,
    /// The leader term the grants are fenced with.
    pub term: u64,
    pub extent_bytes: u64,
    pub ttl_ms: u64,
    /// Replicas each cell must end up with.
    pub needed: usize,
    pub cells: Vec<DirectCell>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectReplicaDone {
    pub node_id: String,
    pub device_index: usize,
    pub offset: u64,
    /// The node's receipt, required when the target was an append.
    #[serde(default)]
    pub receipt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectCellDone {
    pub logical_offset: u64,
    /// Hex SHA-256 of the cell's bytes, computed by the client.
    pub checksum: String,
    pub replicas: Vec<DirectReplicaDone>,
}

/// Body of `POST .../direct-commit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectCommit {
    pub lease: String,
    pub cells: Vec<DirectCellDone>,
    /// Nodes whose write failed: the leader backs them off like a failed leader-side write.
    #[serde(default)]
    pub failed: Vec<String>,
}

#[derive(Debug)]
struct PendingTarget {
    grant_id: String,
    node_id: String,
    device_index: usize,
    /// `None`: an append.
    offset: Option<u64>,
}

#[derive(Debug)]
struct Pending {
    token: u64,
    fs: String,
    ino: u64,
    term: u64,
    extent_bytes: u64,
    needed: usize,
    expires: Instant,
    cells: Vec<(u64, Vec<PendingTarget>)>,
}

#[derive(Debug)]
pub(super) struct DirectWrites {
    ttl: Mutex<(Duration, Duration)>,
    pending: Mutex<HashMap<String, Pending>>,
}

impl Default for DirectWrites {
    fn default() -> Self {
        Self {
            ttl: Mutex::new((DEFAULT_LEASE_TTL, DEFAULT_EXPIRED_GRACE)),
            pending: Mutex::default(),
        }
    }
}

impl DirectWrites {
    fn timing(&self) -> (Duration, Duration) {
        self.ttl
            .lock()
            .map(|t| *t)
            .unwrap_or((DEFAULT_LEASE_TTL, DEFAULT_EXPIRED_GRACE))
    }
}

impl NativeEngine {
    /// Sets how long a direct-write lease lives and how long the ranges of one nobody committed
    /// stay out of the free list afterwards (default 30 s each). The grace must exceed the
    /// longest write a client may still have in flight when its lease expires.
    pub fn set_direct_write_timing(&self, lease_ttl: Duration, expired_grace: Duration) {
        if let Ok(mut t) = self.direct.ttl.lock() {
            *t = (lease_ttl, expired_grace);
        }
    }

    /// Direct-write leases currently outstanding.
    pub fn direct_write_leases(&self) -> usize {
        self.reap_direct();
        self.direct.pending.lock().map(|p| p.len()).unwrap_or(0)
    }

    /// Hands every lease past its expiry to the quarantine.
    pub(super) fn reap_direct(&self) {
        let (_, grace) = self.direct.timing();
        let expired: Vec<Pending> = {
            let Ok(mut p) = self.direct.pending.lock() else {
                return;
            };
            if p.is_empty() {
                return;
            }
            let now = Instant::now();
            let due: Vec<String> = p
                .iter()
                .filter(|(_, l)| l.expires <= now)
                .map(|(id, _)| id.clone())
                .collect();
            due.iter().filter_map(|id| p.remove(id)).collect()
        };
        for l in expired {
            self.adopt(l.token).quarantine(grace);
        }
    }

    fn take_direct(&self, lease: &str) -> Result<Pending, NativeError> {
        self.direct
            .pending
            .lock()
            .map_err(|_| NativeError::Poisoned("direct writes"))?
            .remove(lease)
            .ok_or_else(|| {
                NativeError::Invalid(format!("direct-write lease {lease} is unknown or expired"))
            })
    }

    /// Leases targets for writing `len` bytes at `offset` of file `ino` straight to the data
    /// nodes. `offset` and `len` must be multiples of the filesystem's extent size; the cluster
    /// must store extents as plain replicas (no compression, deduplication or erasure coding
    /// for them) and hold a [`crate::grant::GrantKey`]. Leader only.
    pub fn direct_write_lease(
        &self,
        fs: &str,
        ino: u64,
        offset: u64,
        len: u64,
    ) -> Result<DirectLease, NativeError> {
        writable(fs)?;
        let key = self.cfg.grant_key.as_ref().ok_or_else(|| {
            NativeError::Invalid("direct writes need a write-grant key on the cluster".into())
        })?;
        if self.cfg.compression || self.cfg.dedup {
            return Err(NativeError::Invalid(
                "direct writes are not available with compression or deduplication".into(),
            ));
        }
        let target = Target::File {
            fs,
            ino,
            replica: false,
        };
        let grid = self.with_catalog(|c| {
            target.extent_at(c, 0)?;
            target.grid(c, self.cfg.extent_bytes)
        })??;
        if self
            .cfg
            .erasure
            .is_some_and(|_| grid as usize >= self.cfg.erasure_min_bytes)
        {
            return Err(NativeError::Invalid(
                "direct writes are not available for erasure-coded extents".into(),
            ));
        }
        offset
            .checked_add(len)
            .ok_or_else(|| NativeError::Invalid("write offset overflows".into()))?;
        if len == 0 || !offset.is_multiple_of(grid) || !len.is_multiple_of(grid) {
            return Err(NativeError::Invalid(format!(
                "a direct write covers whole {grid}-byte extents"
            )));
        }
        let cells = (len / grid) as usize;
        if cells > MAX_LEASE_CELLS {
            return Err(NativeError::Invalid(format!(
                "a direct-write lease covers at most {MAX_LEASE_CELLS} extents"
            )));
        }
        // The fence first: on a follower it fails as "not the leader", and on the leader it waits
        // until the catalog has applied every earlier entry (the file's creation).
        let term = {
            let _write = self
                .write_lock
                .lock()
                .map_err(|_| NativeError::Poisoned("write"))?;
            self.write_fence()?
        };
        let (ttl, _) = self.direct.timing();
        let needed = self.cfg.placement.replicas;
        let lease_id = Uuid::new_v4().to_string();
        let exp_ms = now_ms().saturating_add(ttl.as_millis() as u64);
        let alloc = self.lease()?;
        let mut out_cells = Vec::with_capacity(cells);
        let mut pending_cells = Vec::with_capacity(cells);
        for i in 0..cells {
            let logical_offset = offset + i as u64 * grid;
            let order = self.placement_order(grid, |_| false);
            if order.len() < needed {
                return Err(NativeError::InsufficientReplicas {
                    needed,
                    found: order.len(),
                });
            }
            let mut targets = Vec::new();
            let mut held = Vec::new();
            for (n, node_id) in order.iter().take(needed + 1).enumerate() {
                let node = self.node(node_id)?;
                let device_index = node
                    .next_device
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    % node.devices.len();
                let endpoint = node.devices[device_index]
                    .endpoint()
                    .ok_or_else(|| {
                        NativeError::Invalid(format!("node {node_id} has no network endpoint"))
                    })?
                    .to_string();
                let at = alloc.take(node_id, device_index, grid)?;
                let grant_id = format!("{lease_id}.{i}.{n}");
                let grant = key.mint(
                    grant_id.clone(),
                    node_id.clone(),
                    term,
                    self.cfg.meta_group,
                    device_index,
                    at,
                    grid,
                    exp_ms,
                );
                held.push(PendingTarget {
                    grant_id,
                    node_id: node_id.clone(),
                    device_index,
                    offset: at,
                });
                targets.push(DirectTarget {
                    node_id: node_id.clone(),
                    endpoint,
                    group: self.cfg.meta_group,
                    device_index,
                    offset: at,
                    grant,
                });
            }
            out_cells.push(DirectCell {
                logical_offset,
                targets,
            });
            pending_cells.push((logical_offset, held));
        }
        let pending = Pending {
            token: alloc.detach(),
            fs: fs.to_string(),
            ino,
            term,
            extent_bytes: grid,
            needed,
            expires: Instant::now() + ttl,
            cells: pending_cells,
        };
        self.direct
            .pending
            .lock()
            .map_err(|_| NativeError::Poisoned("direct writes"))?
            .insert(lease_id.clone(), pending);
        Ok(DirectLease {
            lease: lease_id,
            term,
            extent_bytes: grid,
            ttl_ms: ttl.as_millis() as u64,
            needed,
            cells: out_cells,
        })
    }

    /// Gives a lease up (the client could not write it). The ranges stay quarantined for the
    /// grace period in case a write is still in flight.
    pub fn direct_write_abort(&self, lease: &str) -> Result<(), NativeError> {
        let (_, grace) = self.direct.timing();
        if let Ok(l) = self.take_direct(lease) {
            self.adopt(l.token).quarantine(grace);
        }
        Ok(())
    }

    /// Commits the extents a client wrote under `lease`. Every cell of the lease must be
    /// reported with at least `needed` replicas the lease allowed (appends proven by the data
    /// node's receipt). The extents' checksums are the client's word; scrub verifies replicas
    /// against them. Leader only; the lease is consumed whatever the outcome.
    pub fn direct_write_commit(
        &self,
        fs: &str,
        ino: u64,
        c: DirectCommit,
    ) -> Result<Attr, NativeError> {
        writable(fs)?;
        self.reap_direct();
        let (_, grace) = self.direct.timing();
        let pending = self.take_direct(&c.lease)?;
        // Dropping `alloc` releases the ranges; an unknown commit outcome quarantines them.
        let alloc = self.adopt(pending.token);
        if pending.fs != fs || pending.ino != ino {
            return Err(NativeError::Invalid(
                "direct-write lease belongs to another file".into(),
            ));
        }
        if pending.expires <= Instant::now() {
            alloc.quarantine(grace);
            return Err(NativeError::Invalid("direct-write lease expired".into()));
        }
        let key = self.cfg.grant_key.as_ref().ok_or_else(|| {
            NativeError::Invalid("direct writes need a write-grant key on the cluster".into())
        })?;
        for node_id in &c.failed {
            let leased = pending
                .cells
                .iter()
                .any(|(_, ts)| ts.iter().any(|t| &t.node_id == node_id));
            if let (true, Ok(node)) = (leased, self.node(node_id)) {
                self.mark_down(node);
                self.telemetry.replica_write_failure();
            }
        }
        if c.cells.len() != pending.cells.len() {
            return Err(NativeError::Invalid(format!(
                "lease covers {} extents, commit reports {}",
                pending.cells.len(),
                c.cells.len()
            )));
        }
        let mut placed = Vec::with_capacity(pending.cells.len());
        for (logical_offset, targets) in &pending.cells {
            let done = c
                .cells
                .iter()
                .find(|d| d.logical_offset == *logical_offset)
                .ok_or_else(|| {
                    NativeError::Invalid(format!("extent at {logical_offset} is not reported"))
                })?;
            let checksum: [u8; 32] = from_hex(&done.checksum)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| NativeError::Invalid("checksum must be 64 hex digits".into()))?;
            let mut replicas: Vec<ReplicaRef> = Vec::new();
            for r in &done.replicas {
                let t = targets
                    .iter()
                    .find(|t| t.node_id == r.node_id && t.device_index == r.device_index)
                    .ok_or_else(|| {
                        NativeError::Invalid(format!(
                            "{} was not leased for the extent at {logical_offset}",
                            r.node_id
                        ))
                    })?;
                match t.offset {
                    Some(leased) if leased == r.offset => {}
                    Some(_) => {
                        return Err(NativeError::Invalid(format!(
                            "{} was leased a different offset",
                            r.node_id
                        )))
                    }
                    None => {
                        let proven = r
                            .receipt
                            .as_deref()
                            .is_some_and(|rc| key.check_receipt(&t.grant_id, r.offset, rc));
                        if !proven {
                            return Err(NativeError::Invalid(format!(
                                "append on {} has no valid receipt",
                                r.node_id
                            )));
                        }
                    }
                }
                if replicas.iter().all(|x| x.node_id != r.node_id) {
                    replicas.push(ReplicaRef {
                        node_id: r.node_id.clone(),
                        device_index: r.device_index,
                        offset: r.offset,
                    });
                }
            }
            if replicas.len() < pending.needed {
                return Err(NativeError::InsufficientReplicas {
                    needed: pending.needed,
                    found: replicas.len(),
                });
            }
            replicas.truncate(pending.needed);
            placed.push(ExtentRef {
                id: Uuid::new_v4().to_string(),
                logical_offset: *logical_offset,
                payload_len: pending.extent_bytes as usize,
                raw_len: 0,
                checksum,
                content: None,
                replicas,
                ec: None,
                created_ms: now_ms(),
                object: None,
            });
        }
        let target = Target::File {
            fs,
            ino,
            replica: false,
        };
        let in_bounds = |cat: &Catalog| target.extent_at(cat, 0).map(|_| ());
        let _write = self
            .write_lock
            .lock()
            .map_err(|_| NativeError::Poisoned("write"))?;
        // The grants were fenced with the lease's term: a new leader must not commit them.
        let term = self.write_fence()?;
        if term != pending.term {
            return Err(NativeError::Fenced { current: term });
        }
        self.with_catalog(in_bounds)??;
        let bytes: u64 = placed.iter().map(|e| e.len() as u64).sum();
        self.commit_cells(&target, placed, Vec::new(), pending.term, &alloc)?;
        self.fs_activity.record_write_unsampled(fs, ino, bytes);
        self.fs_getattr(fs, ino)
    }
}
