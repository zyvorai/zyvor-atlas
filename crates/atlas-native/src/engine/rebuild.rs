// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! What the rebuild controller ([`crate::rebuild`]) asks of an engine: which nodes are lost,
//! which extents that leaves short of parts, and repairing one extent at a time.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Bound,
    time::{Duration, Instant},
};

use super::{NativeEngine, NativeError, RepairStats};
use crate::metadata::ReplicaRef;

/// An extent with parts on lost nodes or failed devices.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Degraded {
    pub extent_id: String,
    /// Parts (replicas or shards) on lost nodes.
    pub lost: usize,
    /// Parts on devices of other nodes that refuse writes. They may still be read, and are
    /// moved.
    pub failed: usize,
    /// How many more parts it can lose and still be read from sound devices: replicas left
    /// minus one, or parity shards minus the parts above.
    pub spare: i64,
    /// Whether a part on a failed device has somewhere to go: a node that is up and not lost,
    /// holds no other part (nor shares a host with one, when placement requires distinct
    /// hosts) and has a device that accepts writes.
    pub movable: bool,
}

impl Degraded {
    /// False if too few parts are left to rebuild it until a node returns.
    pub fn rebuildable(&self) -> bool {
        self.spare + self.failed as i64 >= 0
    }
}

impl NativeEngine {
    /// Nodes configured unhealthy or failing every request for at least `after`.
    pub fn lost_nodes(&self, after: Duration) -> BTreeSet<String> {
        let now = Instant::now();
        self.nodes
            .iter()
            .filter(|n| {
                !n.spec.healthy
                    || n.health
                        .down_since
                        .lock()
                        .ok()
                        .and_then(|d| *d)
                        .is_some_and(|t| now.duration_since(t) >= after)
            })
            .map(|n| n.spec.id.clone())
            .collect()
    }

    /// Asks every configured node not inside a back-off window about each of its devices, so a
    /// node that died while idle is noticed, one that came back is cleared, and devices that
    /// refuse writes are known. Nodes answer in parallel; a dead one costs at most one I/O
    /// timeout per back-off window.
    pub fn probe_nodes(&self) {
        std::thread::scope(|s| {
            for n in self.nodes.iter().filter(|n| self.is_up(n)) {
                s.spawn(move || {
                    if self.probe_node_devices(n) {
                        self.mark_up(n);
                    } else {
                        self.mark_down(n);
                    }
                });
            }
        });
    }

    /// Extents with parts on `lost` nodes, least spare redundancy first. Reads metadata only.
    pub fn degraded_extents(&self, lost: &BTreeSet<String>) -> Result<Vec<Degraded>, NativeError> {
        self.degraded_extents_on(lost, &BTreeSet::new())
    }

    /// [`Self::degraded_extents`], counting parts on `failed` devices
    /// ([`Self::failed_devices`]) as well.
    pub fn degraded_extents_on(
        &self,
        lost: &BTreeSet<String>,
        failed: &BTreeSet<(String, usize)>,
    ) -> Result<Vec<Degraded>, NativeError> {
        if lost.is_empty() && failed.is_empty() {
            return Ok(Vec::new());
        }
        let on_failed = |node: &str, device: usize| failed.contains(&(node.to_string(), device));
        // The nodes a part could move to.
        let targets: Vec<(&str, &str)> = self
            .nodes
            .iter()
            .filter(|n| {
                !failed.is_empty()
                    && !lost.contains(&n.spec.id)
                    && self.is_up(n)
                    && (0..n.devices.len()).any(|i| !on_failed(&n.spec.id, i))
            })
            .map(|n| (n.spec.id.as_str(), n.spec.failure_domain.host.as_str()))
            .collect();
        let hosts: BTreeMap<&str, &str> = self
            .nodes
            .iter()
            .map(|n| (n.spec.id.as_str(), n.spec.failure_domain.host.as_str()))
            .collect();
        let distinct = self.cfg.placement.require_distinct_hosts;
        let mut out: Vec<Degraded> = self.with_catalog(|c| {
            c.extents
                .values()
                .filter_map(|m| {
                    let e = &m.extent;
                    let n = e
                        .replicas
                        .iter()
                        .filter(|r| lost.contains(&r.node_id))
                        .count();
                    let moving: Vec<&ReplicaRef> = e
                        .replicas
                        .iter()
                        .filter(|r| {
                            !lost.contains(&r.node_id) && on_failed(&r.node_id, r.device_index)
                        })
                        .collect();
                    if n + moving.len() == 0 {
                        return None;
                    }
                    let movable = moving.iter().any(|part| {
                        targets.iter().any(|(node, host)| {
                            e.replicas.iter().filter(|r| r != part).all(|r| {
                                r.node_id != *node
                                    && !(distinct && hosts.get(r.node_id.as_str()) == Some(host))
                            })
                        })
                    });
                    let tolerance = match &e.ec {
                        Some(ec) => ec.parity,
                        None => e.replicas.len().saturating_sub(1),
                    };
                    Some(Degraded {
                        extent_id: e.id.clone(),
                        lost: n,
                        failed: moving.len(),
                        spare: tolerance as i64 - (n + moving.len()) as i64,
                        movable,
                    })
                })
                .collect()
        })?;
        out.sort_by(|a, b| {
            a.spare
                .cmp(&b.spare)
                .then_with(|| a.extent_id.cmp(&b.extent_id))
        });
        Ok(out)
    }

    /// Up to `n` extent ids after `after` (from the start when `None`), in id order.
    pub fn extent_ids_after(
        &self,
        after: Option<&str>,
        n: usize,
    ) -> Result<Vec<String>, NativeError> {
        self.with_catalog(|c| {
            let lower = after.map_or(Bound::Unbounded, |a| Bound::Excluded(a.to_string()));
            c.extents
                .range::<String, _>((lower, Bound::Unbounded))
                .take(n)
                .map(|(id, _)| id.clone())
                .collect()
        })
    }

    /// Checks one extent and rebuilds its bad parts onto other nodes; parts on `lost` nodes
    /// are rebuilt without being read. Under Raft only the leader can repair.
    pub fn repair_extent(
        &self,
        id: &str,
        lost: &BTreeSet<String>,
    ) -> Result<RepairStats, NativeError> {
        self.repair_extent_on(id, lost, &BTreeSet::new())
    }

    /// [`Self::repair_extent`] that also moves parts off `failed` devices. A part stays where it
    /// is until its replacement is committed.
    pub fn repair_extent_on(
        &self,
        id: &str,
        lost: &BTreeSet<String>,
        failed: &BTreeSet<(String, usize)>,
    ) -> Result<RepairStats, NativeError> {
        let fence = self.write_fence()?;
        let _repair = self
            .repair_lock
            .lock()
            .map_err(|_| NativeError::Poisoned("repair"))?;
        let mut st = RepairStats::default();
        self.repair_extent_with(id, lost, failed, fence, &mut st)?;
        Ok(st)
    }
}
