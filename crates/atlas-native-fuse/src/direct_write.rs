// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Direct writes: whole extents go from this client straight to the data nodes; the metadata
//! leader only leases the targets and commits (`POST .../direct-write`, `.../direct-commit`).
//! Anything else (the unaligned head and tail of a run, a lease or commit that fails, a data
//! node that is down with no spare, a fenced term, a cluster without write grants) is written
//! through the leader as before, so enabling this never makes a write fail that would have
//! succeeded.
//!
//! The client holds no standing write credential: each data-node write carries a short-lived
//! grant the leader minted for exactly that range, length and term.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use atlas_native::{
    checksum,
    engine::{
        DirectCell, DirectCellDone, DirectCommit, DirectLease, DirectReplicaDone, DirectTarget,
    },
    NativeError, RemoteDevice, TlsIdentity,
};
use reqwest::Method;
use serde_json::{json, Value};

use crate::{
    client::{Body, Client, Retry},
    locks::Locks,
};

/// Extents leased per request (the cluster allows more).
const CELLS_PER_LEASE: usize = 16;
/// Extents written concurrently, each to all its replicas in parallel.
const CELL_PARALLELISM: usize = 4;

/// Write file data straight to the data nodes; see the module docs.
#[derive(Debug, Clone)]
pub struct DirectWrites {
    /// Client identity for data nodes that require mutual TLS; plaintext otherwise.
    pub identity: Option<Arc<TlsIdentity>>,
    pub timeout: Duration,
}

/// `(node id, endpoint, device, group)`.
type NodeKey = (String, String, usize, u32);

/// Writes `(offset, bytes)` through the leader; returns the file's attributes.
pub(crate) trait Fallback<E>: Fn(u64, &[u8]) -> Result<Option<Value>, E> {}
impl<E, F: Fn(u64, &[u8]) -> Result<Option<Value>, E>> Fallback<E> for F {}

/// Why a direct write did not happen; the caller writes through the leader instead.
type Failure = String;

pub(crate) struct DirectWriter {
    client: Arc<Client>,
    fs: String,
    cfg: DirectWrites,
    locks: Arc<Locks>,
    /// The filesystem's extent size, once known.
    grid: Mutex<Option<u64>>,
    nodes: Mutex<HashMap<NodeKey, Arc<RemoteDevice>>>,
    /// Set when the cluster refused to lease (direct writes off or not possible for this
    /// filesystem): chunks go straight to the leader until then instead of asking every time.
    paused_until: Mutex<Option<Instant>>,
    pub direct: AtomicU64,
    pub fallbacks: AtomicU64,
}

/// How long a refused lease request pauses further attempts.
const PAUSE: Duration = Duration::from_secs(60);

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl DirectWriter {
    pub fn new(client: Arc<Client>, fs: &str, cfg: DirectWrites, locks: Arc<Locks>) -> Self {
        Self {
            client,
            fs: fs.to_string(),
            cfg,
            locks,
            grid: Mutex::new(None),
            nodes: Mutex::new(HashMap::new()),
            paused_until: Mutex::new(None),
            direct: AtomicU64::new(0),
            fallbacks: AtomicU64::new(0),
        }
    }

    /// The filesystem's extent size.
    fn grid(&self) -> Result<u64, Failure> {
        if let Some(g) = self.grid.lock().map_err(|_| "poisoned")?.as_ref() {
            return Ok(*g);
        }
        let v = self
            .client
            .json(Method::GET, "/v1/fs", Body::Empty, Retry::Idempotent)
            .map_err(|e| e.to_string())?;
        let grid = v["filesystems"]
            .as_array()
            .and_then(|all| all.iter().find(|f| f["id"] == self.fs.as_str()))
            .and_then(|f| f["extent_bytes"].as_u64())
            .filter(|g| *g > 0)
            .ok_or("filesystem extent size unknown")?;
        *self.grid.lock().map_err(|_| "poisoned")? = Some(grid);
        Ok(grid)
    }

    fn path(&self, ino: u64, op: &str) -> String {
        crate::ops::fs_path(&self.fs, &self.locks, &format!("/inodes/{ino}/{op}"))
    }

    /// Splits `[offset, offset + len)` into the unaligned head, the whole-extent body and the
    /// tail, as `(head, body)` lengths. `None` when no whole extent is covered.
    pub fn split(&self, offset: u64, len: usize) -> Option<(usize, usize)> {
        let grid = self.grid().ok()?;
        let head = ((grid - offset % grid) % grid) as usize;
        let rest = len.checked_sub(head)?;
        let body = rest - rest % grid as usize;
        (body > 0).then_some((head, body))
    }

    /// Writes the whole extents `data` covers at `offset` (a multiple of the extent size, and
    /// `data` a multiple of it), in leases of up to [`CELLS_PER_LEASE`] extents. A chunk that
    /// cannot go direct is handed to `fallback` (the leader path); returns the last reply.
    pub fn write<E>(
        &self,
        ino: u64,
        offset: u64,
        data: &[u8],
        fallback: &dyn Fallback<E>,
    ) -> Result<Option<Value>, E> {
        let grid = match self.grid() {
            Ok(g) => g as usize,
            Err(_) => return fallback(offset, data),
        };
        let mut last = None;
        for (i, chunk) in data.chunks(grid * CELLS_PER_LEASE).enumerate() {
            let at = offset + (i * grid * CELLS_PER_LEASE) as u64;
            let paused = self
                .paused_until
                .lock()
                .is_ok_and(|p| p.is_some_and(|t| t > Instant::now()));
            let tried = if paused {
                Err("direct writes paused: the cluster refused a lease".to_string())
            } else {
                self.write_chunk(ino, at, chunk)
            };
            match tried {
                Ok(v) => {
                    self.direct.fetch_add(1, Ordering::Relaxed);
                    last = Some(v);
                }
                Err(why) => {
                    tracing::debug!(ino, offset = at, error = %why, "direct write fell back");
                    self.fallbacks.fetch_add(1, Ordering::Relaxed);
                    last = fallback(at, chunk)?;
                }
            }
        }
        Ok(last)
    }

    fn write_chunk(&self, ino: u64, offset: u64, data: &[u8]) -> Result<Value, Failure> {
        let v = self
            .client
            .json(
                Method::POST,
                &self.path(ino, "direct-write"),
                Body::Json(json!({ "offset": offset, "len": data.len() })),
                Retry::Idempotent,
            )
            .map_err(|e| {
                // "invalid" is the cluster saying direct writes can't be used here (no grant
                // key, compression, erasure coding, ...): stop asking for a while.
                if matches!(&e, crate::client::Error::Api { code, .. } if code == "invalid") {
                    if let Ok(mut p) = self.paused_until.lock() {
                        *p = Some(Instant::now() + PAUSE);
                    }
                }
                format!("lease: {e}")
            })?;
        let lease: DirectLease =
            serde_json::from_value(v).map_err(|e| format!("lease reply: {e}"))?;
        match self.write_lease(&lease, offset, data) {
            Ok(commit) => self
                .client
                .json(
                    Method::POST,
                    &self.path(ino, "direct-commit"),
                    Body::Json(json!(commit)),
                    // A repeated commit is refused (the lease is consumed), which falls back
                    // to rewriting the chunk through the leader: same bytes, same place.
                    Retry::Idempotent,
                )
                .map_err(|e| format!("commit: {e}")),
            Err(why) => {
                // Free the ranges early instead of waiting for the lease to expire.
                let _ = self.client.request(
                    Method::POST,
                    &self.path(ino, "direct-abort"),
                    Body::Json(json!({ "lease": lease.lease })),
                    Retry::Idempotent,
                );
                Err(why)
            }
        }
    }

    fn write_lease(
        &self,
        lease: &DirectLease,
        offset: u64,
        data: &[u8],
    ) -> Result<DirectCommit, Failure> {
        let grid = lease.extent_bytes as usize;
        if lease.cells.len() * grid != data.len()
            || lease.cells.first().map(|c| c.logical_offset) != Some(offset)
        {
            return Err("lease does not match the write".into());
        }
        let cells: Vec<(&DirectCell, &[u8])> = lease.cells.iter().zip(data.chunks(grid)).collect();
        let mut done = Vec::with_capacity(cells.len());
        let mut failed: Vec<String> = Vec::new();
        for window in cells.chunks(CELL_PARALLELISM) {
            let results: Vec<Result<(DirectCellDone, Vec<String>), Failure>> =
                std::thread::scope(|s| {
                    let handles: Vec<_> = window
                        .iter()
                        .map(|(cell, bytes)| s.spawn(|| self.write_cell(lease, cell, bytes)))
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().unwrap_or_else(|_| Err("writer panicked".into())))
                        .collect()
                });
            for r in results {
                let (d, f) = r?;
                done.push(d);
                failed.extend(f);
            }
        }
        failed.sort();
        failed.dedup();
        Ok(DirectCommit {
            lease: lease.lease.clone(),
            cells: done,
            failed,
        })
    }

    /// Writes one extent to `needed` replicas in parallel, then to spares for any that failed.
    fn write_cell(
        &self,
        lease: &DirectLease,
        cell: &DirectCell,
        data: &[u8],
    ) -> Result<(DirectCellDone, Vec<String>), Failure> {
        let needed = lease.needed.min(cell.targets.len());
        let (primary, spares) = cell.targets.split_at(needed);
        let results: Vec<Result<DirectReplicaDone, NativeError>> = std::thread::scope(|s| {
            let handles: Vec<_> = primary
                .iter()
                .map(|t| s.spawn(|| self.write_target(lease.term, t, data)))
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(NativeError::Remote("writer panicked".into())))
                })
                .collect()
        });
        let mut replicas = Vec::new();
        let mut failed = Vec::new();
        let mut spares = spares.iter();
        let mut outcomes: Vec<(&DirectTarget, Result<DirectReplicaDone, NativeError>)> =
            primary.iter().zip(results).collect();
        loop {
            for (t, r) in outcomes.drain(..) {
                match r {
                    Ok(d) => replicas.push(d),
                    // A later term owns the data nodes: nothing this lease wrote may commit.
                    Err(e @ NativeError::Fenced { .. }) => return Err(e.to_string()),
                    Err(e) => {
                        tracing::debug!(node = %t.node_id, error = %e, "direct replica write failed");
                        failed.push(t.node_id.clone());
                    }
                }
            }
            if replicas.len() >= needed {
                break;
            }
            let Some(spare) = spares.next() else {
                return Err(format!(
                    "only {} of {needed} replicas written",
                    replicas.len()
                ));
            };
            outcomes.push((spare, self.write_target(lease.term, spare, data)));
        }
        Ok((
            DirectCellDone {
                logical_offset: cell.logical_offset,
                checksum: hex(&checksum::sha256(data)),
                replicas,
            },
            failed,
        ))
    }

    fn write_target(
        &self,
        term: u64,
        t: &DirectTarget,
        data: &[u8],
    ) -> Result<DirectReplicaDone, NativeError> {
        let dev = self.device(t)?;
        let (offset, receipt) = match t.offset {
            Some(off) => {
                dev.write_at_granted(Some(t.grant.clone()), term, off, data)?;
                (off, None)
            }
            None => dev.append_granted(Some(t.grant.clone()), term, data)?,
        };
        Ok(DirectReplicaDone {
            node_id: t.node_id.clone(),
            device_index: t.device_index,
            offset,
            receipt,
        })
    }

    fn device(&self, t: &DirectTarget) -> Result<Arc<RemoteDevice>, NativeError> {
        let mut nodes = self
            .nodes
            .lock()
            .map_err(|_| NativeError::Poisoned("data-node cache"))?;
        let key = (
            t.node_id.clone(),
            t.endpoint.clone(),
            t.device_index,
            t.group,
        );
        if let Some(d) = nodes.get(&key) {
            return Ok(d.clone());
        }
        let dev = match &self.cfg.identity {
            Some(id) => RemoteDevice::with_tls(&t.endpoint, &t.node_id, id, self.cfg.timeout)?,
            None => RemoteDevice::new(&t.endpoint, self.cfg.timeout),
        };
        let dev = Arc::new(dev.on_device(t.device_index).in_group(t.group));
        nodes.insert(key, dev.clone());
        Ok(dev)
    }
}
