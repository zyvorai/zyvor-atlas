// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! What the rebuild controller ([`crate::rebuild`]) asks of an engine: which nodes are lost,
//! which extents that leaves short of parts, and repairing one extent at a time.

use std::{
    collections::BTreeSet,
    ops::Bound,
    time::{Duration, Instant},
};

use super::{NativeEngine, NativeError, RepairStats};

/// An extent with parts on lost nodes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Degraded {
    pub extent_id: String,
    /// Parts (replicas or shards) on lost nodes.
    pub lost: usize,
    /// How many more parts it can lose and still be read: replicas left minus one, or parity
    /// shards minus lost shards. Negative means it can't be rebuilt until a node returns.
    pub spare: i64,
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

    /// Asks every configured node not inside a back-off window for its device length, so a
    /// node that died while idle is noticed and one that came back is cleared. Nodes answer in
    /// parallel; a dead one costs at most one I/O timeout per back-off window.
    pub fn probe_nodes(&self) {
        std::thread::scope(|s| {
            for n in self.nodes.iter().filter(|n| self.is_up(n)) {
                let Some(dev) = n.devices.first() else {
                    continue;
                };
                s.spawn(move || match dev.len() {
                    Ok(_) => self.mark_up(n),
                    Err(_) => self.mark_down(n),
                });
            }
        });
    }

    /// Extents with parts on `lost` nodes, least spare redundancy first. Reads metadata only.
    pub fn degraded_extents(&self, lost: &BTreeSet<String>) -> Result<Vec<Degraded>, NativeError> {
        if lost.is_empty() {
            return Ok(Vec::new());
        }
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
                    if n == 0 {
                        return None;
                    }
                    let tolerance = match &e.ec {
                        Some(ec) => ec.parity,
                        None => e.replicas.len().saturating_sub(1),
                    };
                    Some(Degraded {
                        extent_id: e.id.clone(),
                        lost: n,
                        spare: tolerance as i64 - n as i64,
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
        let fence = self.write_fence()?;
        let _repair = self
            .repair_lock
            .lock()
            .map_err(|_| NativeError::Poisoned("repair"))?;
        let mut st = RepairStats::default();
        self.repair_extent_with(id, lost, fence, &mut st)?;
        Ok(st)
    }
}
