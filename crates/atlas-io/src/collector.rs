// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use atlas_api_types::{BioEvent, IoHistogram, IoNative, IoRca, IoSensorHealth, IoWorkload};

use crate::aishape;
use crate::devmap::DeviceMap;
use crate::heat::{HeatSet, IoHeatmap};
use crate::hist::HistSet;
use crate::lease::LeaseTable;
use crate::rca;
use crate::source::IoSource;
use crate::CARDINALITY_CAP;

struct WorkAcc {
    ops: u64,
    bytes: u64,
    sum_us: u64,
    comm: String,
    device: String,
    volume_id: Option<String>,
    pid: u32,
    cgroup_id: u64,
}

struct Inner {
    source: Box<dyn IoSource>,
    map: DeviceMap,
    hist: HistSet,
    heat: HeatSet,
    work: HashMap<(u64, u32, u64), WorkAcc>,
    leases: LeaseTable,
    seen: u64,
}

pub struct Collector {
    inner: Mutex<Inner>,
}

impl Collector {
    pub fn new(source: Box<dyn IoSource>, map: DeviceMap) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                source,
                map,
                hist: HistSet::default(),
                heat: HeatSet::default(),
                work: HashMap::new(),
                leases: LeaseTable::default(),
                seen: 0,
            }),
        })
    }

    pub fn poll(&self) {
        let mut g = self.inner.lock().expect("collector lock");
        let evs = g.source.poll_events();
        let now = unix_now();
        for ev in evs {
            g.seen += 1;
            g.hist.record(&ev);
            g.heat.record(&ev, now);
            Self::record_work(&mut g, &ev);
        }
    }

    fn record_work(g: &mut Inner, ev: &BioEvent) {
        if g.work.len() as u64 >= CARDINALITY_CAP
            && !g.work.contains_key(&(ev.cgroup_id, ev.pid, ev.dev_key()))
        {
            return;
        }
        let bind = g.map.resolve(ev.major, ev.minor);
        let acc = g
            .work
            .entry((ev.cgroup_id, ev.pid, ev.dev_key()))
            .or_insert(WorkAcc {
                ops: 0,
                bytes: 0,
                sum_us: 0,
                comm: ev.comm.clone(),
                device: bind.name.clone(),
                volume_id: bind.volume_id.clone(),
                pid: ev.pid,
                cgroup_id: ev.cgroup_id,
            });
        acc.ops += 1;
        acc.bytes += ev.bytes as u64;
        acc.sum_us += ev.latency_us();
    }

    pub fn histograms(&self) -> Vec<IoHistogram> {
        let g = self.inner.lock().expect("collector lock");
        g.hist.snapshots(|maj, min| {
            let b = g.map.resolve(maj, min);
            (b.name, b.volume_id)
        })
    }

    /// Per-minute latency columns from the last `minutes` minutes.
    pub fn heatmap(&self, minutes: u64) -> Vec<IoHeatmap> {
        let g = self.inner.lock().expect("collector lock");
        let since = unix_now().saturating_sub(minutes.saturating_mul(crate::heat::COLUMN_SECS));
        g.heat.snapshots(since, |maj, min| {
            let b = g.map.resolve(maj, min);
            (b.name, b.volume_id)
        })
    }

    pub fn workloads(&self) -> Vec<IoWorkload> {
        let g = self.inner.lock().expect("collector lock");
        let mut v: Vec<_> = g
            .work
            .values()
            .map(|w| IoWorkload {
                cgroup_id: w.cgroup_id,
                pid: w.pid,
                comm: w.comm.clone(),
                device: w.device.clone(),
                volume_id: w.volume_id.clone(),
                ops: w.ops,
                bytes: w.bytes,
                sum_us: w.sum_us,
            })
            .collect();
        v.sort_by_key(|a| std::cmp::Reverse(a.bytes));
        v
    }

    pub fn health(&self) -> IoSensorHealth {
        let g = self.inner.lock().expect("collector lock");
        g.source.health(
            g.seen,
            g.hist.dropped(),
            g.hist.cardinality() + g.work.len() as u64,
        )
    }

    /// Atlas Native I/O with device names resolved; `None` when the native maps are not loaded.
    pub fn native(&self) -> Option<IoNative> {
        let g = self.inner.lock().expect("collector lock");
        let mut n = g.source.native()?;
        for s in &mut n.stats {
            s.device = g.map.resolve(s.major, s.minor).name;
        }
        for s in &mut n.slow {
            s.device = g.map.resolve(s.major, s.minor).name;
        }
        Some(n)
    }

    /// Per-device I/O shape (checkpoint-like writes, bulk reads, small reads); see [`aishape`].
    pub fn ai_shapes(&self) -> Vec<aishape::IoShape> {
        aishape::classify(&self.histograms())
    }

    pub fn rca(&self, volume: Option<&str>) -> Vec<IoRca> {
        let h = self.histograms();
        let w = self.workloads();
        rca::explain(&h, &w, volume)
    }

    pub fn grant_lease(
        &self,
        device: String,
        volume_id: Option<String>,
        ttl_secs: u64,
        reason: String,
    ) -> Result<atlas_api_types::IoLease, String> {
        let mut g = self.inner.lock().expect("collector lock");
        g.leases.grant(device, volume_id, ttl_secs, reason, None)
    }

    pub fn leases(&self) -> Vec<atlas_api_types::IoLease> {
        self.inner
            .lock()
            .expect("collector lock")
            .leases
            .active(None)
    }

    pub fn seen(&self) -> u64 {
        self.inner.lock().expect("collector lock").seen
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::FakeSource;

    #[test]
    fn demo_pipeline_attributes_qemu() {
        let c = Collector::new(Box::new(FakeSource::demo()), DeviceMap::lab());
        c.poll();
        assert_eq!(c.seen(), 101);
        let ws = c.workloads();
        assert!(
            ws.iter()
                .any(|w| w.comm == "qemu-system-x86"
                    && w.volume_id.as_deref() == Some("vol_vm_web01"))
        );
        let rca = c.rca(Some("vol_vm_web01"));
        assert!(rca.iter().any(|r| r.verdict == "critical_latency"));
    }
}
