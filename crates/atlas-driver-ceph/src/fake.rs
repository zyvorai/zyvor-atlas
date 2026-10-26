// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Fake Ceph driver returning deterministic fixtures. Used for local dev, integration tests, and
//! demos where no Ceph cluster is reachable (`ATLAS_CEPH_DRIVER_MODE=fake`).

use async_trait::async_trait;
use atlas_api_types::{
    DiscoveryResult, Health, MetricSample, Osd, StorageCluster, StorageHealth, StoragePool,
    StorageVolume, VolumeKind,
};
use atlas_driver_core::{DriverError, StorageDriver};

pub struct FakeCephDriver {
    backend_id: String,
    cluster_id: String,
}

impl FakeCephDriver {
    pub fn new(backend_id: impl Into<String>) -> Self {
        let backend_id = backend_id.into();
        Self {
            cluster_id: "cls_fake0001".into(),
            backend_id,
        }
    }

    fn cluster(&self) -> StorageCluster {
        StorageCluster {
            id: self.cluster_id.clone(),
            backend_id: self.backend_id.clone(),
            name: "zyvor-ceph-lab".into(),
            native_fsid: Some("f5100000-0000-4000-8000-000000000001".into()),
            // Kept in sync with ceph_status()/ceph_osd_tree()/ceph_osd_df() below, which all
            // hardcode osd.3 as down — this fixture must agree so Command Deck (fed by
            // discover()/cluster()/osds()) and the /ceph page (fed by the ceph_status() family)
            // never disagree about cluster health for the same simulated cluster.
            health: Health::Warn,
            raw_capacity_bytes: Some(220_000_000_000_000),
            used_capacity_bytes: Some(97_000_000_000_000),
            available_capacity_bytes: Some(123_000_000_000_000),
        }
    }

    fn pools(&self) -> Vec<StoragePool> {
        vec![
            StoragePool {
                id: "pool_rbd_nvme_prod".into(),
                cluster_id: self.cluster_id.clone(),
                name: "rbd-nvme-prod".into(),
                kind: "rbd".into(),
                device_class: Some("nvme".into()),
                replica_size: Some(3),
                used_bytes: Some(40_000_000_000_000),
                max_bytes: Some(80_000_000_000_000),
                health: Health::Ok,
            },
            StoragePool {
                id: "pool_cephfs_data0".into(),
                cluster_id: self.cluster_id.clone(),
                name: "cephfs-data0".into(),
                kind: "cephfs_data".into(),
                device_class: Some("ssd".into()),
                replica_size: Some(3),
                used_bytes: Some(12_000_000_000_000),
                max_bytes: Some(60_000_000_000_000),
                health: Health::Ok,
            },
            StoragePool {
                id: "pool_rgw_root".into(),
                cluster_id: self.cluster_id.clone(),
                name: ".rgw.root".into(),
                kind: "rgw".into(),
                device_class: Some("hdd".into()),
                replica_size: Some(3),
                used_bytes: Some(1_000_000_000),
                max_bytes: Some(40_000_000_000_000),
                health: Health::Ok,
            },
        ]
    }

    fn osds(&self) -> Vec<Osd> {
        (0..6)
            .map(|id| Osd {
                id,
                cluster_id: self.cluster_id.clone(),
                // osd.3 matches the "down" OSD hardcoded in ceph_status()/ceph_osd_tree()/
                // ceph_osd_df() below — one fixture, reused everywhere, so every endpoint agrees.
                up: id != 3,
                in_cluster: true,
                device_class: Some(if id < 3 { "nvme" } else { "ssd" }.into()),
                host: Some(format!("node0{}", (id / 2) + 1)),
                used_bytes: if id == 3 {
                    Some(0)
                } else {
                    Some(16_000_000_000_000)
                },
                capacity_bytes: Some(36_000_000_000_000),
            })
            .collect()
    }
}

#[async_trait]
impl StorageDriver for FakeCephDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    async fn estimate_dedup(
        &self,
        pool: &str,
        opts: atlas_api_types::DedupEstimateOptions,
    ) -> Result<atlas_api_types::DedupEstimate, DriverError> {
        if !self.list_pools().await?.iter().any(|p| p.name == pool && p.kind == "rbd") {
            return Err(DriverError::Backend(format!("invalid: '{pool}' is not an RBD pool on this cluster")));
        }
        // Fixture data run through the real scanner: eight VM-like images that share a base layer
        // (identical blocks) plus a unique tail each, and some holes.
        let bs = opts.block_size.clamp(4 << 10, 16 << 20) as usize;
        let layer = |seed: u8| -> Vec<u8> {
            (0..bs).map(|i| (i as u8).wrapping_mul(seed | 1).wrapping_add(seed)).collect()
        };
        let mut scanner = atlas_driver_core::dedup::DedupScanner::new(
            opts.block_size,
            atlas_driver_core::dedup::DEFAULT_MAX_UNIQUE,
        );
        for image in 0..8u8 {
            for base in 1..=6u8 {
                scanner.feed(&layer(base));
            }
            for tail in 0..2u8 {
                scanner.feed(&layer(100 + image * 2 + tail));
            }
            scanner.feed_zeros(32 * bs as u64);
        }
        let total = scanner.examined_bytes();
        let scan = scanner.finish();
        Ok(atlas_driver_core::dedup::estimate_from_scan(&self.backend_id, pool, &scan, Some(total)))
    }

    fn is_fixture(&self) -> bool {
        true
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        Ok(DiscoveryResult {
            cluster: self.cluster(),
            pools: self.pools(),
            osds: self.osds(),
            volumes: self.list_volumes("rbd-nvme-prod").await?,
            health: self.health().await?,
        })
    }

    async fn health(&self) -> Result<StorageHealth, DriverError> {
        Ok(StorageHealth {
            // Matches cluster()/osds() (osd.3 down) and ceph_status()'s 8
            // active+undersized+degraded PGs — one simulated cluster state, not two.
            status: Health::Warn,
            summary: "HEALTH_WARN: 1 osds down".into(),
            raw_capacity_bytes: Some(220_000_000_000_000),
            used_capacity_bytes: Some(97_000_000_000_000),
            available_capacity_bytes: Some(123_000_000_000_000),
            recovering: false,
            degraded_objects: 8,
        })
    }

    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        Ok(self.pools())
    }

    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        // Only the block pool has RBD images in the fixture.
        if pool != "rbd-nvme-prod" {
            return Ok(vec![]);
        }
        Ok(vec![
            StorageVolume {
                id: "vol_rbd_nvme_prod_billing-db-01-root".into(),
                cluster_id: Some(self.cluster_id.clone()),
                pool_id: Some("pool_rbd_nvme_prod".into()),
                name: "billing-db-01-root".into(),
                kind: VolumeKind::Block,
                backend_native_id: Some("rbd:rbd-nvme-prod/billing-db-01-root".into()),
                size_bytes: 536_870_912_000,
                used_bytes: Some(211_312_390_144),
                state: "available".into(),
                health: Health::Ok,
                kubernetes_namespace: Some("tenant-acme".into()),
                pvc_name: Some("pvc-billing-db-01-root".into()),
                storage_class_name: Some("zyvor-rbd-prod".into()),
            },
            StorageVolume {
                id: "vol_rbd_nvme_prod_web-01-root".into(),
                cluster_id: Some(self.cluster_id.clone()),
                pool_id: Some("pool_rbd_nvme_prod".into()),
                name: "web-01-root".into(),
                kind: VolumeKind::Block,
                backend_native_id: Some("rbd:rbd-nvme-prod/web-01-root".into()),
                size_bytes: 42_949_672_960,
                used_bytes: Some(9_000_000_000),
                state: "available".into(),
                health: Health::Ok,
                kubernetes_namespace: Some("tenant-acme".into()),
                pvc_name: Some("pvc-web-01-root".into()),
                storage_class_name: Some("zyvor-rbd-prod".into()),
            },
        ])
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        let m = |name: &str, value: f64| MetricSample {
            name: name.into(),
            value,
            labels: Default::default(),
        };
        Ok(vec![
            m("ceph_cluster_total_bytes", 220_000_000_000_000.0),
            m("ceph_cluster_used_bytes", 97_000_000_000_000.0),
            m("ceph_cluster_avail_bytes", 123_000_000_000_000.0),
            m("ceph_degraded_objects", 0.0),
        ])
    }

    async fn ceph_status(&self) -> Result<serde_json::Value, DriverError> {
        Ok(serde_json::json!({
            "fsid": "f5100000-0000-4000-8000-000000000001",
            "health": { "status": "HEALTH_WARN", "checks": {
                "OSD_DOWN": { "severity": "HEALTH_WARN", "summary": { "message": "1 osds down" } }
            }},
            "monmap": { "num_mons": 3 },
            "quorum_names": ["a", "b", "c"],
            "mgrmap": { "available": true, "active_name": "a" },
            "osdmap": { "num_osds": 6, "num_up_osds": 5, "num_in_osds": 6 },
            "pgmap": {
                "num_pgs": 289,
                "pgs_by_state": [ { "state_name": "active+clean", "count": 281 }, { "state_name": "active+undersized+degraded", "count": 8 } ],
                "bytes_total": 220_000_000_000_000_i64, "bytes_used": 97_000_000_000_000_i64, "bytes_avail": 123_000_000_000_000_i64,
                "read_bytes_sec": 12_400_000, "write_bytes_sec": 8_100_000, "read_op_per_sec": 1420, "write_op_per_sec": 860,
                "recovering_bytes_per_sec": 0
            }
        }))
    }

    async fn ceph_osd_tree(&self) -> Result<serde_json::Value, DriverError> {
        let osd = |id: i64, up: bool| serde_json::json!({ "id": id, "name": format!("osd.{id}"), "type": "osd", "status": if up {"up"} else {"down"}, "crush_weight": 32.7, "reweight": if up {1.0} else {0.0} });
        Ok(serde_json::json!({ "nodes": [
            { "id": -1, "name": "default", "type": "root", "children": [-2, -3, -4] },
            { "id": -2, "name": "node01", "type": "host", "children": [0, 1] }, osd(0, true), osd(1, true),
            { "id": -3, "name": "node02", "type": "host", "children": [2, 3] }, osd(2, true), osd(3, false),
            { "id": -4, "name": "node03", "type": "host", "children": [4, 5] }, osd(4, true), osd(5, true)
        ]}))
    }

    async fn ceph_df(&self) -> Result<serde_json::Value, DriverError> {
        let pool = |id: i64, name: &str, stored: i64, objs: i64, pct: f64| serde_json::json!({ "name": name, "id": id, "stats": { "stored": stored, "objects": objs, "percent_used": pct, "max_avail": 80_000_000_000_000_i64 } });
        Ok(serde_json::json!({
            "stats": { "total_bytes": 220_000_000_000_000_i64, "total_used_bytes": 97_000_000_000_000_i64, "total_avail_bytes": 123_000_000_000_000_i64 },
            "pools": [ pool(1, "rbd-nvme-prod", 40_000_000_000_000, 9_800_000, 0.33), pool(2, "cephfs-data0", 12_000_000_000_000, 3_100_000, 0.16), pool(3, ".rgw.root", 1_000_000_000, 42, 0.0) ]
        }))
    }

    async fn ceph_osd_df(&self) -> Result<serde_json::Value, DriverError> {
        let kib = 36_000_000_000_i64; // ~36 TB per OSD in KiB
        let o = |id: i64, host: &str, pct: f64, pgs: i64, up: bool| {
            serde_json::json!({
                "id": id, "name": format!("osd.{id}"), "device_class": if id < 3 {"nvme"} else {"ssd"},
                "kb": kib, "kb_used": (kib as f64 * pct / 100.0) as i64, "kb_avail": (kib as f64 * (1.0 - pct / 100.0)) as i64,
                "utilization": pct, "pgs": pgs, "status": if up {"up"} else {"down"}, "crush_weight": 32.7, "host": host
            })
        };
        Ok(serde_json::json!({
            "nodes": [
                o(0, "node01", 41.2, 96, true), o(1, "node01", 44.8, 101, true),
                o(2, "node02", 39.5, 92, true), o(3, "node02", 0.0, 0, false),
                o(4, "node03", 47.1, 108, true), o(5, "node03", 43.3, 99, true)
            ],
            "summary": { "total_kb": kib * 6, "average_utilization": 42.6, "min_var": 0.93, "max_var": 1.11, "dev": 2.8 }
        }))
    }

    /// Fixture model of Ceph's answer for the topology above (osd 0-1 on node01, 2-3 on node02,
    /// 4-5 on node03, osd.3 already down, three replicas across the hosts): stopping is safe only
    /// while it leaves at most one host impaired. node02 already is, so stopping osd.2 is fine
    /// and stopping anything on node01 or node03 is not.
    async fn ceph_osd_ok_to_stop(&self, osd_ids: &[i64]) -> Result<serde_json::Value, DriverError> {
        let host_of = |id: i64| match id {
            0 | 1 => Some("node01"),
            2 | 3 => Some("node02"),
            4 | 5 => Some("node03"),
            _ => None,
        };
        let mut impaired = std::collections::BTreeSet::from(["node02"]);
        for id in osd_ids {
            let host = host_of(*id)
                .ok_or_else(|| DriverError::Backend(format!("osd.{id} does not exist")))?;
            impaired.insert(host);
        }
        let ok = impaired.len() <= 1;
        Ok(serde_json::json!({
            "ok_to_stop": ok,
            "osds": osd_ids,
            "num_ok_pgs": if ok { 289 } else { 192 },
            "num_not_ok_pgs": if ok { 0 } else { 97 },
            "bad_become_inactive": [],
        }))
    }

    /// One drive per OSD above. `life_expectancy_*` are empty when Ceph has no prediction; the
    /// stamp dates are far enough out or in the past to keep the fixture independent of today.
    async fn ceph_device_ls(&self) -> Result<serde_json::Value, DriverError> {
        let dev = |id: &str, host: &str, osd: i64, min: &str, max: &str| {
            serde_json::json!({
                "devid": id,
                "location": [{ "host": host, "dev": "nvme0n1", "path": "/dev/nvme0n1" }],
                "daemons": [format!("osd.{osd}")],
                "life_expectancy_min": min,
                "life_expectancy_max": max,
                "life_expectancy_stamp": if min.is_empty() { "" } else { "2026-10-01T00:00:00.000000+0000" },
            })
        };
        Ok(serde_json::json!([
            dev("INTEL_SSDPE2KX040T8_PHLJ0001", "node01", 0, "", ""),
            dev("INTEL_SSDPE2KX040T8_PHLJ0002", "node01", 1, "2099-01-01T00:00:00.000000+0000", "2100-01-01T00:00:00.000000+0000"),
            dev("SAMSUNG_MZQL23T8HCLS_S64E0001", "node02", 2, "", ""),
            dev("SAMSUNG_MZQL23T8HCLS_S64E0002", "node02", 3, "2020-01-01T00:00:00.000000+0000", "2020-06-01T00:00:00.000000+0000"),
            dev("MICRON_7450_MTFDKCC3T8TFR_0001", "node03", 4, "", ""),
            dev("MICRON_7450_MTFDKCC3T8TFR_0002", "node03", 5, "", ""),
        ]))
    }
}
