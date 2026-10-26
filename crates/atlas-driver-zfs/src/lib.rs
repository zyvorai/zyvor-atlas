// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! ZFS storage driver — a third backend behind the `StorageDriver` trait, showing the pluggable
//! architecture scales past Ceph + NFS.
//!
//! It models a ZFS host as a one-cluster backend: each **zpool** is a `StoragePool`
//! (`kind = "zpool"`) and each dataset under it is a filesystem `StorageVolume`.
//!
//! Two implementations, selected by `ATLAS_ZFS_DRIVER_MODE` (`fake`, the default, or `real`) —
//! mirrors `atlas-driver-ceph`'s `FakeCephDriver`/`RealCephDriver` split:
//! - [`FakeZfsDriver`] reports deterministic capacity fixtures — no command execution at all,
//!   safe for tests/CI/demos without real ZFS infra.
//! - [`RealZfsDriver`] runs `zpool list -Hp` / `zfs list -Hp` — **locally**, on whatever host the
//!   gateway process itself runs on. ZFS has no standard remote query protocol (unlike Ceph's
//!   `ceph`/`rbd` CLI, which talks to mons over the network); a genuinely remote ZFS host needs
//!   SSH or a vendor management API, neither of which this driver implements yet — `host` is kept
//!   for cosmetic naming/`backend_native_id` only. A configured zpool that doesn't actually exist
//!   locally is dropped, never fabricated. Security rule (PDF §17.3, same as the Ceph driver's
//!   `cmd.rs`): every command is an argument array, never a shell string.

use async_trait::async_trait;
use atlas_api_types::{
    DiscoveryResult, Health, MetricSample, StorageCluster, StorageHealth, StoragePool,
    StorageVolume, VolumeKind,
};
use atlas_driver_core::{DriverError, StorageDriver};

mod cmd;
pub mod dedup;
pub use cmd::{
    cluster_id, inspect_device, list_pool_datasets, list_whole_disks, list_zpool_names,
    non_root_datasets, pool_id, root_volume_id, wipe_device, zpool_create, zpool_destroy,
    BlockDevice, DeviceCheck,
};

pub(crate) fn sanitize(s: &str) -> String {
    s.trim_matches('/')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

// ---------------------------------------------------------------------------
// Fake (fixture) driver
// ---------------------------------------------------------------------------

/// Per-zpool capacity fixture (bytes).
const POOL_CAPACITY: i64 = 4_000_000_000_000; // 4 TB
const POOL_USED: i64 = 1_600_000_000_000; // 40% used

pub struct FakeZfsDriver {
    backend_id: String,
    host: String,
    zpools: Vec<String>,
}

impl FakeZfsDriver {
    pub fn new(
        backend_id: impl Into<String>,
        host: impl Into<String>,
        zpools: Vec<String>,
    ) -> Self {
        Self {
            backend_id: backend_id.into(),
            host: host.into(),
            zpools,
        }
    }

    pub fn with_defaults(backend_id: impl Into<String>) -> Self {
        Self::new(
            backend_id,
            "zfs01.zyvor.lab",
            vec!["tank".into(), "vault".into()],
        )
    }

    fn cluster_id(&self) -> String {
        format!("cls_zfs_{}", sanitize(&self.host))
    }

    fn cluster(&self) -> StorageCluster {
        let n = self.zpools.len().max(1) as i64;
        StorageCluster {
            id: self.cluster_id(),
            backend_id: self.backend_id.clone(),
            name: format!("zfs://{}", self.host),
            native_fsid: None,
            health: Health::Ok,
            raw_capacity_bytes: Some(POOL_CAPACITY * n),
            used_capacity_bytes: Some(POOL_USED * n),
            available_capacity_bytes: Some((POOL_CAPACITY - POOL_USED) * n),
        }
    }

    fn pool_for(&self, zpool: &str) -> StoragePool {
        StoragePool {
            id: format!("pool_zfs_{}", sanitize(zpool)),
            cluster_id: self.cluster_id(),
            name: zpool.to_string(),
            kind: "zpool".into(),
            device_class: None,
            replica_size: None,
            used_bytes: Some(POOL_USED),
            max_bytes: Some(POOL_CAPACITY),
            health: Health::Ok,
        }
    }

    fn pools(&self) -> Vec<StoragePool> {
        self.zpools.iter().map(|z| self.pool_for(z)).collect()
    }
}

#[async_trait]
impl StorageDriver for FakeZfsDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    async fn estimate_dedup(
        &self,
        pool: &str,
        _opts: atlas_api_types::DedupEstimateOptions,
    ) -> Result<atlas_api_types::DedupEstimate, DriverError> {
        // Fixture: the recorded `zdb -S` output run through the real parser.
        let sim = dedup::parse_zdb_simulation(dedup::SAMPLE_ZDB_OUTPUT)?;
        Ok(dedup::estimate(&self.backend_id, pool, sim, None))
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        let mut volumes = Vec::new();
        for z in &self.zpools {
            volumes.extend(self.list_volumes(z).await?);
        }
        Ok(DiscoveryResult {
            cluster: self.cluster(),
            pools: self.pools(),
            osds: vec![],
            volumes,
            health: self.health().await?,
        })
    }

    async fn health(&self) -> Result<StorageHealth, DriverError> {
        let n = self.zpools.len().max(1) as i64;
        Ok(StorageHealth {
            status: Health::Ok,
            summary: format!("{} zpool(s) ONLINE on {}", self.zpools.len(), self.host),
            raw_capacity_bytes: Some(POOL_CAPACITY * n),
            used_capacity_bytes: Some(POOL_USED * n),
            available_capacity_bytes: Some((POOL_CAPACITY - POOL_USED) * n),
            recovering: false,
            degraded_objects: 0,
        })
    }

    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        Ok(self.pools())
    }

    /// One dataset per zpool (a real driver enumerates `zfs list` datasets/zvols).
    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        if !self.zpools.iter().any(|z| z == pool) {
            return Ok(vec![]);
        }
        let p = sanitize(pool);
        Ok(vec![StorageVolume {
            id: format!("vol_zfs_{p}"),
            cluster_id: Some(self.cluster_id()),
            pool_id: Some(format!("pool_zfs_{p}")),
            name: format!("{pool}/data"),
            kind: VolumeKind::Filesystem,
            backend_native_id: Some(format!("{}:{}/data", self.host, pool)),
            size_bytes: POOL_CAPACITY,
            used_bytes: Some(POOL_USED),
            state: "available".into(),
            health: Health::Ok,
            kubernetes_namespace: None,
            pvc_name: None,
            storage_class_name: None,
        }])
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        let n = self.zpools.len().max(1) as f64;
        let m = |name: &str, value: f64| MetricSample {
            name: name.into(),
            value,
            labels: Default::default(),
        };
        Ok(vec![
            m("zfs_zpools_total", self.zpools.len() as f64),
            m("zfs_capacity_bytes", POOL_CAPACITY as f64 * n),
            m("zfs_used_bytes", POOL_USED as f64 * n),
        ])
    }
}

// ---------------------------------------------------------------------------
// Real driver
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct PoolCapacity {
    size_bytes: i64,
    alloc_bytes: i64,
}

struct Dataset {
    name: String,
    used_bytes: i64,
    mountpoint: Option<String>,
}

pub struct RealZfsDriver {
    backend_id: String,
    host: String,
    zpools: Vec<String>,
}

impl RealZfsDriver {
    pub fn new(
        backend_id: impl Into<String>,
        host: impl Into<String>,
        zpools: Vec<String>,
    ) -> Self {
        Self {
            backend_id: backend_id.into(),
            host: host.into(),
            zpools,
        }
    }

    fn cluster_id(&self) -> String {
        format!("cls_zfs_{}", sanitize(&self.host))
    }

    /// `zpool list -Hp -o name,size,alloc` (local execution — see module doc for why). Errors
    /// (binary missing, no zpools importable) become `DriverError::Unreachable`, matching
    /// `RealCephDriver`'s convention of propagating rather than silently degrading.
    async fn zpool_list(&self) -> Result<std::collections::HashMap<String, PoolCapacity>, DriverError> {
        let output = tokio::process::Command::new("zpool")
            .args(["list", "-Hp", "-o", "name,size,alloc"])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| DriverError::Unreachable(format!("failed to spawn `zpool`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(DriverError::Unreachable(format!(
                "zpool list -Hp: {stderr}"
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .filter_map(|line| {
                let mut cols = line.split('\t');
                let name = cols.next()?.to_string();
                let size_bytes = cols.next()?.parse().ok()?;
                let alloc_bytes = cols.next()?.parse().ok()?;
                Some((
                    name,
                    PoolCapacity {
                        size_bytes,
                        alloc_bytes,
                    },
                ))
            })
            .collect())
    }

    /// This backend's configured zpools that actually exist locally (via `zpool list`) — never a
    /// configured pool that isn't really there, never a real pool nobody configured.
    async fn reachable_pools(
        &self,
    ) -> Result<std::collections::HashMap<String, PoolCapacity>, DriverError> {
        let real = self.zpool_list().await?;
        Ok(real
            .into_iter()
            .filter(|(name, _)| self.zpools.iter().any(|z| z == name))
            .collect())
    }

    /// `zfs list -Hp -o name,used,mountpoint -r <pool>` — every dataset under the pool (the pool's
    /// own root dataset included, matching what `zfs list` reports by default).
    async fn datasets(&self, pool: &str) -> Result<Vec<Dataset>, DriverError> {
        let output = tokio::process::Command::new("zfs")
            .args(["list", "-Hp", "-o", "name,used,mountpoint", "-r", pool])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| DriverError::Unreachable(format!("failed to spawn `zfs`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(DriverError::Backend(format!(
                "zfs list -Hp -r {pool}: {stderr}"
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .filter_map(|line| {
                let mut cols = line.split('\t');
                let name = cols.next()?.to_string();
                let used_bytes = cols.next()?.parse().ok()?;
                let mountpoint = cols.next().map(str::to_string).filter(|m| m != "-");
                Some(Dataset {
                    name,
                    used_bytes,
                    mountpoint,
                })
            })
            .collect())
    }

    fn cluster(
        &self,
        pools: &std::collections::HashMap<String, PoolCapacity>,
    ) -> StorageCluster {
        let raw: i64 = pools.values().map(|p| p.size_bytes).sum();
        let used: i64 = pools.values().map(|p| p.alloc_bytes).sum();
        let any = !pools.is_empty();
        StorageCluster {
            id: self.cluster_id(),
            backend_id: self.backend_id.clone(),
            name: format!("zfs://{}", self.host),
            native_fsid: None,
            health: Health::Ok,
            raw_capacity_bytes: any.then_some(raw),
            used_capacity_bytes: any.then_some(used),
            available_capacity_bytes: any.then_some(raw - used),
        }
    }

    fn pool_for(&self, name: &str, cap: PoolCapacity) -> StoragePool {
        StoragePool {
            id: format!("pool_zfs_{}", sanitize(name)),
            cluster_id: self.cluster_id(),
            name: name.to_string(),
            kind: "zpool".into(),
            device_class: None,
            replica_size: None,
            used_bytes: Some(cap.alloc_bytes),
            max_bytes: Some(cap.size_bytes),
            health: Health::Ok,
        }
    }
}

#[async_trait]
impl StorageDriver for RealZfsDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    async fn estimate_dedup(
        &self,
        pool: &str,
        opts: atlas_api_types::DedupEstimateOptions,
    ) -> Result<atlas_api_types::DedupEstimate, DriverError> {
        // Only pools this driver was configured with: the name goes straight to `zdb`.
        if !self.zpools.iter().any(|p| p == pool) {
            return Err(DriverError::Backend(format!("not found: unknown zpool '{pool}'")));
        }
        let run = tokio::process::Command::new("zdb")
            .args(["-S", pool])
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(std::time::Duration::from_secs(opts.max_secs.max(1)), run)
            .await
            .map_err(|_| {
                DriverError::Backend(format!(
                    "zdb -S {pool} did not finish within {}s; it reads every block, so raise max_secs for a large pool",
                    opts.max_secs
                ))
            })?
            .map_err(|e| DriverError::Unreachable(format!("failed to spawn `zdb`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(DriverError::Backend(format!("zdb -S {pool}: {stderr}")));
        }
        let sim = dedup::parse_zdb_simulation(&String::from_utf8_lossy(&output.stdout))?;
        let alloc = self.zpool_list().await.ok().and_then(|m| m.get(pool).map(|c| c.alloc_bytes.max(0) as u64));
        Ok(dedup::estimate(&self.backend_id, pool, sim, alloc))
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        let pools = self.reachable_pools().await?;
        let mut volumes = Vec::new();
        for name in pools.keys() {
            volumes.extend(self.list_volumes(name).await?);
        }
        Ok(DiscoveryResult {
            cluster: self.cluster(&pools),
            pools: pools
                .iter()
                .map(|(name, cap)| self.pool_for(name, *cap))
                .collect(),
            osds: vec![],
            volumes,
            health: self.health().await?,
        })
    }

    async fn health(&self) -> Result<StorageHealth, DriverError> {
        let pools = self.reachable_pools().await?;
        let raw: i64 = pools.values().map(|p| p.size_bytes).sum();
        let used: i64 = pools.values().map(|p| p.alloc_bytes).sum();
        let any = !pools.is_empty();
        Ok(StorageHealth {
            status: Health::Ok,
            summary: format!("{} zpool(s) ONLINE on {}", pools.len(), self.host),
            raw_capacity_bytes: any.then_some(raw),
            used_capacity_bytes: any.then_some(used),
            available_capacity_bytes: any.then_some(raw - used),
            recovering: false,
            degraded_objects: 0,
        })
    }

    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        let pools = self.reachable_pools().await?;
        Ok(pools
            .iter()
            .map(|(name, cap)| self.pool_for(name, *cap))
            .collect())
    }

    /// One `StorageVolume` per real ZFS dataset under `pool` (via `zfs list -r`), not the Fake
    /// driver's single cosmetic `<pool>/data` placeholder.
    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        let pools = self.reachable_pools().await?;
        if !pools.contains_key(pool) {
            return Ok(vec![]);
        }
        let datasets = self.datasets(pool).await?;
        Ok(datasets
            .into_iter()
            .map(|d| {
                let id = sanitize(&d.name);
                StorageVolume {
                    id: format!("vol_zfs_{id}"),
                    cluster_id: Some(self.cluster_id()),
                    pool_id: Some(format!("pool_zfs_{}", sanitize(pool))),
                    name: d.name.clone(),
                    kind: VolumeKind::Filesystem,
                    backend_native_id: Some(format!("{}:{}", self.host, d.name)),
                    size_bytes: d.used_bytes,
                    used_bytes: Some(d.used_bytes),
                    state: "available".into(),
                    health: Health::Ok,
                    kubernetes_namespace: None,
                    pvc_name: None,
                    storage_class_name: d.mountpoint,
                }
            })
            .collect())
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        let pools = self.reachable_pools().await?;
        let raw: i64 = pools.values().map(|p| p.size_bytes).sum();
        let used: i64 = pools.values().map(|p| p.alloc_bytes).sum();
        let m = |name: &str, value: f64| MetricSample {
            name: name.into(),
            value,
            labels: Default::default(),
        };
        let mut out = vec![m("zfs_zpools_total", pools.len() as f64)];
        if !pools.is_empty() {
            out.push(m("zfs_capacity_bytes", raw as f64));
            out.push(m("zfs_used_bytes", used as f64));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_discover_maps_zpools_to_pools_and_datasets() {
        let d = FakeZfsDriver::new(
            "bkd_zfs",
            "zfs01.example.com",
            vec!["tank".into(), "vault".into()],
        );
        let r = d.discover().await.unwrap();
        assert_eq!(r.pools.len(), 2);
        assert_eq!(r.pools[0].kind, "zpool");
        assert_eq!(r.volumes.len(), 2);
        assert!(matches!(r.volumes[0].kind, VolumeKind::Filesystem));
        assert_eq!(r.cluster.raw_capacity_bytes, Some(POOL_CAPACITY * 2));
    }

    #[tokio::test]
    async fn fake_unknown_pool_has_no_volumes() {
        let d = FakeZfsDriver::with_defaults("bkd_zfs");
        assert!(d.list_volumes("nope").await.unwrap().is_empty());
    }

    /// `zpool`/`zfs` binaries aren't installed in CI, so every real-command call surfaces as
    /// `DriverError::Unreachable` — the correct, honest behavior (no fabricated fallback data).
    #[tokio::test]
    async fn real_missing_zpool_binary_errors_instead_of_fabricating_data() {
        let d = RealZfsDriver::new("bkd_zfs", "localhost", vec!["tank".into()]);
        let err = d.discover().await.unwrap_err();
        assert!(matches!(err, DriverError::Unreachable(_)), "{err:?}");
    }

    #[tokio::test]
    async fn real_list_volumes_also_requires_the_pool_to_exist() {
        let d = RealZfsDriver::new("bkd_zfs", "localhost", vec!["tank".into()]);
        let err = d.list_volumes("tank").await.unwrap_err();
        assert!(matches!(err, DriverError::Unreachable(_)), "{err:?}");
    }

    #[test]
    fn sanitize_strips_non_alphanumerics() {
        assert_eq!(sanitize("tank/data"), "tank_data");
    }
}
