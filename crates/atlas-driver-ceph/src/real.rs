// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Real Ceph driver: parses `ceph`/`rbd` CLI JSON into Atlas DTOs.

use async_trait::async_trait;
use atlas_api_types::{
    DiscoveryResult, Health, MetricSample, Osd, StorageCluster, StorageHealth, StoragePool,
    StorageVolume, VolumeKind,
};
use atlas_driver_core::{DriverError, StorageDriver};

use crate::cmd::{ceph_cmd, rbd_cmd, rbd_du_pool};

/// Live Ceph backend accessed through the `ceph`/`rbd` CLIs.
pub struct RealCephDriver {
    backend_id: String,
}

impl RealCephDriver {
    pub fn new(backend_id: impl Into<String>) -> Self {
        Self {
            backend_id: backend_id.into(),
        }
    }

    async fn cluster_from_status(&self) -> Result<(StorageCluster, StorageHealth), DriverError> {
        let status = ceph_cmd(&["status"]).await?;
        let df = ceph_cmd(&["df", "detail"]).await?;

        let fsid = status
            .get("fsid")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let health = map_health(
            status
                .pointer("/health/status")
                .and_then(|v| v.as_str())
                .unwrap_or("HEALTH_UNKNOWN"),
        );

        let stats = df.get("stats");
        let raw = stats
            .and_then(|s| s.get("total_bytes"))
            .and_then(|v| v.as_i64());
        let used = stats
            .and_then(|s| s.get("total_used_bytes"))
            .and_then(|v| v.as_i64());
        let avail = stats
            .and_then(|s| s.get("total_avail_bytes"))
            .and_then(|v| v.as_i64());

        let recovering = status
            .pointer("/pgmap/recovering_bytes_per_sec")
            .and_then(|v| v.as_i64())
            .map(|b| b > 0)
            .unwrap_or(false);
        let degraded = status
            .pointer("/pgmap/degraded_objects")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let summary = status
            .pointer("/health/status")
            .and_then(|v| v.as_str())
            .unwrap_or("HEALTH_UNKNOWN")
            .to_string();

        let cluster = StorageCluster {
            id: format!("cls_{fsid}"),
            backend_id: self.backend_id.clone(),
            name: self.backend_id.clone(),
            native_fsid: Some(fsid),
            health,
            raw_capacity_bytes: raw,
            used_capacity_bytes: used,
            available_capacity_bytes: avail,
        };
        let storage_health = StorageHealth {
            status: health,
            summary,
            raw_capacity_bytes: raw,
            used_capacity_bytes: used,
            available_capacity_bytes: avail,
            recovering,
            degraded_objects: degraded,
        };
        Ok((cluster, storage_health))
    }

    async fn pools_for(&self, cluster_id: &str) -> Result<Vec<StoragePool>, DriverError> {
        let df = ceph_cmd(&["df", "detail"]).await?;
        let empty = vec![];
        let pools = df.get("pools").and_then(|v| v.as_array()).unwrap_or(&empty);
        Ok(pools
            .iter()
            .map(|p| {
                let name = p
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let id = p
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .map(|n| format!("pool_{n}"))
                    .unwrap_or_else(|| format!("pool_{name}"));
                StoragePool {
                    id,
                    cluster_id: cluster_id.to_string(),
                    kind: pool_kind_from_name(&name),
                    name,
                    device_class: None,
                    replica_size: None,
                    used_bytes: p.pointer("/stats/bytes_used").and_then(|v| v.as_i64()),
                    max_bytes: p.pointer("/stats/max_avail").and_then(|v| v.as_i64()),
                    health: Health::Ok,
                }
            })
            .collect())
    }

    async fn osds_for(&self, cluster_id: &str) -> Result<Vec<Osd>, DriverError> {
        let tree = ceph_cmd(&["osd", "tree"]).await?;
        let empty = vec![];
        let nodes = tree
            .get("nodes")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty);
        let host_of = osd_host_map(nodes);
        Ok(nodes
            .iter()
            .filter(|n| n.get("type").and_then(|v| v.as_str()) == Some("osd"))
            .filter_map(|n| {
                let id = n.get("id").and_then(|v| v.as_i64())?;
                let up = n.get("status").and_then(|v| v.as_str()) == Some("up");
                Some(Osd {
                    id,
                    cluster_id: cluster_id.to_string(),
                    up,
                    in_cluster: n
                        .get("reweight")
                        .and_then(|v| v.as_f64())
                        .map(|r| r > 0.0)
                        .unwrap_or(up),
                    device_class: n
                        .get("device_class")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    host: host_of.get(&id).cloned(),
                    used_bytes: None,
                    capacity_bytes: None,
                })
            })
            .collect())
    }
}

#[async_trait]
impl StorageDriver for RealCephDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        let (cluster, health) = self.cluster_from_status().await?;
        let pools = self.pools_for(&cluster.id).await?;
        let osds = self.osds_for(&cluster.id).await?;

        // Enumerate RBD images in each rbd-kind pool, reconciling each volume's foreign keys to
        // the pool/cluster ids the inventory will store (list_volumes only knows the pool *name*).
        let mut volumes = Vec::new();
        for pool in pools.iter().filter(|p| p.kind == "rbd") {
            let mut vols = self.list_volumes(&pool.name).await.unwrap_or_default();
            for v in &mut vols {
                v.pool_id = Some(pool.id.clone());
                v.cluster_id = Some(cluster.id.clone());
            }
            volumes.extend(vols);
        }

        Ok(DiscoveryResult {
            cluster,
            pools,
            osds,
            volumes,
            health,
        })
    }

    async fn health(&self) -> Result<StorageHealth, DriverError> {
        Ok(self.cluster_from_status().await?.1)
    }

    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        let (cluster, _) = self.cluster_from_status().await?;
        self.pools_for(&cluster.id).await
    }

    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        let out = rbd_cmd(&["ls", "-l", pool]).await?;
        let empty = vec![];
        let images = out.as_array().unwrap_or(&empty);
        // `rbd ls -l` has no `used_size` field (only `size`, the provisioned size); fetch actual
        // usage separately via `rbd du`.
        let used: std::collections::HashMap<String, i64> = rbd_du_pool(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();
        Ok(images
            .iter()
            // `rbd ls -l` also lists snapshot rows (with a "snapshot" field); keep base images.
            .filter(|img| img.get("snapshot").is_none())
            .filter_map(|img| {
                let name = img.get("image").and_then(|v| v.as_str())?.to_string();
                let size = img.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
                Some(StorageVolume {
                    id: format!("vol_{pool}_{name}"),
                    cluster_id: None,
                    pool_id: Some(format!("pool_{pool}")),
                    backend_native_id: Some(format!("rbd:{pool}/{name}")),
                    size_bytes: size,
                    used_bytes: used.get(&name).copied(),
                    name,
                    kind: VolumeKind::Block,
                    state: "available".into(),
                    health: Health::Ok,
                    kubernetes_namespace: None,
                    pvc_name: None,
                    storage_class_name: None,
                })
            })
            .collect())
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        let (_, h) = self.cluster_from_status().await?;
        let mut out = Vec::new();
        if let Some(v) = h.raw_capacity_bytes {
            out.push(metric("ceph_cluster_total_bytes", v as f64));
        }
        if let Some(v) = h.used_capacity_bytes {
            out.push(metric("ceph_cluster_used_bytes", v as f64));
        }
        if let Some(v) = h.available_capacity_bytes {
            out.push(metric("ceph_cluster_avail_bytes", v as f64));
        }
        out.push(metric("ceph_degraded_objects", h.degraded_objects as f64));
        Ok(out)
    }

    async fn ceph_status(&self) -> Result<serde_json::Value, DriverError> {
        ceph_cmd(&["status"]).await
    }
    async fn ceph_osd_tree(&self) -> Result<serde_json::Value, DriverError> {
        ceph_cmd(&["osd", "tree"]).await
    }
    async fn ceph_df(&self) -> Result<serde_json::Value, DriverError> {
        ceph_cmd(&["df", "detail"]).await
    }
    async fn ceph_osd_df(&self) -> Result<serde_json::Value, DriverError> {
        ceph_cmd(&["osd", "df"]).await
    }
    async fn ceph_osd_ok_to_stop(&self, osd_ids: &[i64]) -> Result<serde_json::Value, DriverError> {
        crate::cmd::ceph_osd_ok_to_stop(osd_ids).await
    }
    async fn ceph_device_ls(&self) -> Result<serde_json::Value, DriverError> {
        ceph_cmd(&["device", "ls"]).await
    }
    async fn ceph_device_health(&self, devid: &str) -> Result<serde_json::Value, DriverError> {
        ceph_cmd(&["device", "get-health-metrics", devid]).await
    }
}

fn metric(name: &str, value: f64) -> MetricSample {
    MetricSample {
        name: name.to_string(),
        value,
        labels: Default::default(),
    }
}

fn map_health(s: &str) -> Health {
    match s {
        "HEALTH_OK" => Health::Ok,
        "HEALTH_WARN" => Health::Warn,
        "HEALTH_ERR" => Health::Critical,
        _ => Health::Unknown,
    }
}

/// Build an OSD-id → host-name lookup from `ceph osd tree`'s flat `nodes` array. Each `host`-typed
/// node lists its OSD ids in `children`; individual `osd`-typed nodes carry no host field of their
/// own, so the host has to be resolved by scanning the host nodes (mirrors the CRUSH hierarchy the
/// fake driver's fixture already models in `FakeCephDriver::osds`/`ceph_osd_tree`).
fn osd_host_map(nodes: &[serde_json::Value]) -> std::collections::HashMap<i64, String> {
    let mut host_of = std::collections::HashMap::new();
    for n in nodes {
        if n.get("type").and_then(|v| v.as_str()) != Some("host") {
            continue;
        }
        let Some(host_name) = n.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(children) = n.get("children").and_then(|v| v.as_array()) {
            for child in children {
                if let Some(id) = child.as_i64() {
                    host_of.insert(id, host_name.to_string());
                }
            }
        }
    }
    host_of
}

/// Heuristic pool classification from its name — the fallback used when no more precise source is
/// available (non-Rook Ceph, or no k8s driver attached). On a Rook-managed cluster,
/// `atlas-discovery::run_discovery`'s `rook_pool_kinds` enrichment overrides this with an exact
/// classification read from live `CephBlockPool`/`CephFilesystem`/`CephObjectStore` CRs
/// (`atlas_driver_k8s::rook::known_rook_pool_kinds`) after this heuristic runs, since this crate
/// has no Kubernetes API access of its own (see `crates/atlas-driver-k8s/src/rook.rs`).
fn pool_kind_from_name(name: &str) -> String {
    let n = name.to_lowercase();
    if n.contains("cephfs") && n.contains("meta") {
        "cephfs_metadata".into()
    } else if n.contains("cephfs") {
        "cephfs_data".into()
    } else if n.contains("rgw") || n.starts_with(".rgw") {
        "rgw".into()
    } else if n == "device_health_metrics" || n.starts_with('.') {
        "other".into()
    } else {
        "rbd".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_kind_classification() {
        assert_eq!(pool_kind_from_name("rbd-nvme-prod"), "rbd");
        assert_eq!(pool_kind_from_name("cephfs-data0"), "cephfs_data");
        assert_eq!(pool_kind_from_name("cephfs-metadata"), "cephfs_metadata");
        assert_eq!(pool_kind_from_name(".rgw.root"), "rgw");
    }

    #[test]
    fn health_mapping() {
        assert_eq!(map_health("HEALTH_OK"), Health::Ok);
        assert_eq!(map_health("HEALTH_WARN"), Health::Warn);
        assert_eq!(map_health("HEALTH_ERR"), Health::Critical);
        assert_eq!(map_health("nonsense"), Health::Unknown);
    }

    /// Regression test for a real/fake parity gap: `osds_for` used to always leave `Osd.host`
    /// `None` because the flat `ceph osd tree` "osd" nodes carry no host field of their own — only
    /// the fake driver's fixture populated it, so `GET /nodes` (which derives storage nodes from
    /// distinct OSD hosts) silently returned nothing against a real cluster while every gateway
    /// test (which only exercises `FakeCephDriver`) passed.
    #[test]
    fn osd_host_map_resolves_hosts_from_crush_tree() {
        let tree = serde_json::json!({ "nodes": [
            { "id": -1, "name": "default", "type": "root", "children": [-2, -3] },
            { "id": -2, "name": "node01", "type": "host", "children": [0, 1] },
            { "id": 0, "name": "osd.0", "type": "osd", "status": "up" },
            { "id": 1, "name": "osd.1", "type": "osd", "status": "up" },
            { "id": -3, "name": "node02", "type": "host", "children": [2] },
            { "id": 2, "name": "osd.2", "type": "osd", "status": "down" }
        ]});
        let nodes = tree["nodes"].as_array().unwrap();
        let hosts = osd_host_map(nodes);
        assert_eq!(hosts.get(&0).map(String::as_str), Some("node01"));
        assert_eq!(hosts.get(&1).map(String::as_str), Some("node01"));
        assert_eq!(hosts.get(&2).map(String::as_str), Some("node02"));
        assert_eq!(hosts.len(), 3);
    }
}
