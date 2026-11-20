// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! NFS storage driver — a second backend behind the `StorageDriver` trait, proving Atlas's
//! pluggable-driver architecture beyond Ceph.
//!
//! It models an NFS server as a one-cluster backend: each **export** is a `StoragePool`
//! (`kind = "nfs_export"`) and each mountable **share** under it is a filesystem `StorageVolume`.
//!
//! Two implementations, selected by `ATLAS_NFS_DRIVER_MODE` (`fake`, the default, or `real`) —
//! mirrors `atlas-driver-ceph`'s `FakeCephDriver`/`RealCephDriver` split:
//! - [`FakeNfsDriver`] reports deterministic capacity fixtures — no network access at all, safe
//!   for tests/CI/demos without real NFS infra.
//! - [`RealNfsDriver`] shells out to `showmount -e <server>` (an RPC to the server's mountd, no
//!   mount performed) to prove the server is reachable and get its real export list — a
//!   configured export the server doesn't actually have is dropped, never fabricated. Per-export
//!   capacity comes from `df` on the export's mount point *when it happens to already be locally
//!   mounted* (checked via `/proc/mounts`, Linux-only — matches the container runtime this ships
//!   in); NFS has no standard remote "capacity without mounting" RPC, so an export that isn't
//!   locally mounted reports `None` capacity rather than a made-up number. Security rule (PDF
//!   §17.3, same as the Ceph driver's `cmd.rs`): every command is an argument array, never a
//!   shell string.

use async_trait::async_trait;
use atlas_api_types::{
    DiscoveryResult, Health, MetricSample, StorageCluster, StorageHealth, StoragePool,
    StorageVolume, VolumeKind,
};
use atlas_driver_core::{DriverError, StorageDriver};

/// Slugify a path/host into an id-safe token.
fn sanitize(s: &str) -> String {
    s.trim_matches('/')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

// ---------------------------------------------------------------------------
// Fake (fixture) driver
// ---------------------------------------------------------------------------

/// Per-export capacity fixture (bytes).
const EXPORT_CAPACITY: i64 = 8_000_000_000_000; // 8 TB
const EXPORT_USED: i64 = 2_400_000_000_000; // 30% used

pub struct FakeNfsDriver {
    backend_id: String,
    server: String,
    exports: Vec<String>,
}

impl FakeNfsDriver {
    /// Build from an NFS server host and its exported paths.
    pub fn new(
        backend_id: impl Into<String>,
        server: impl Into<String>,
        exports: Vec<String>,
    ) -> Self {
        Self {
            backend_id: backend_id.into(),
            server: server.into(),
            exports,
        }
    }

    /// Demo fixture used when NFS is enabled without an explicit server/exports.
    pub fn with_defaults(backend_id: impl Into<String>) -> Self {
        Self::new(
            backend_id,
            "nfs01.zyvor.lab",
            vec!["/exports/vmstore".into(), "/exports/backups".into()],
        )
    }

    fn cluster_id(&self) -> String {
        format!("cls_nfs_{}", sanitize(&self.server))
    }

    fn cluster(&self) -> StorageCluster {
        let n = self.exports.len().max(1) as i64;
        StorageCluster {
            id: self.cluster_id(),
            backend_id: self.backend_id.clone(),
            name: format!("nfs://{}", self.server),
            native_fsid: None,
            health: Health::Ok,
            raw_capacity_bytes: Some(EXPORT_CAPACITY * n),
            used_capacity_bytes: Some(EXPORT_USED * n),
            available_capacity_bytes: Some((EXPORT_CAPACITY - EXPORT_USED) * n),
        }
    }

    fn pool_for(&self, export: &str) -> StoragePool {
        StoragePool {
            id: format!("pool_nfs_{}", sanitize(export)),
            cluster_id: self.cluster_id(),
            name: export.to_string(),
            kind: "nfs_export".into(),
            device_class: None,
            replica_size: None,
            used_bytes: Some(EXPORT_USED),
            max_bytes: Some(EXPORT_CAPACITY),
            health: Health::Ok,
        }
    }

    fn pools(&self) -> Vec<StoragePool> {
        self.exports.iter().map(|e| self.pool_for(e)).collect()
    }
}

#[async_trait]
impl StorageDriver for FakeNfsDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        let mut volumes = Vec::new();
        for export in &self.exports {
            volumes.extend(self.list_volumes(export).await?);
        }
        Ok(DiscoveryResult {
            cluster: self.cluster(),
            pools: self.pools(),
            osds: vec![], // NFS has no OSDs.
            volumes,
            health: self.health().await?,
        })
    }

    async fn health(&self) -> Result<StorageHealth, DriverError> {
        let n = self.exports.len().max(1) as i64;
        Ok(StorageHealth {
            status: Health::Ok,
            summary: format!(
                "{} export(s) reachable on {}",
                self.exports.len(),
                self.server
            ),
            raw_capacity_bytes: Some(EXPORT_CAPACITY * n),
            used_capacity_bytes: Some(EXPORT_USED * n),
            available_capacity_bytes: Some((EXPORT_CAPACITY - EXPORT_USED) * n),
            recovering: false,
            degraded_objects: 0,
        })
    }

    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        Ok(self.pools())
    }

    /// One mountable filesystem share per export (a real driver enumerates subdirectories).
    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        // `pool` is the export path (matches StoragePool.name).
        if !self.exports.iter().any(|e| e == pool) {
            return Ok(vec![]);
        }
        let share = sanitize(pool);
        Ok(vec![StorageVolume {
            id: format!("vol_nfs_{share}"),
            cluster_id: Some(self.cluster_id()),
            pool_id: Some(format!("pool_nfs_{share}")),
            name: format!("{pool}/share"),
            kind: VolumeKind::Filesystem,
            backend_native_id: Some(format!("{}:{}", self.server, pool)),
            size_bytes: EXPORT_CAPACITY,
            used_bytes: Some(EXPORT_USED),
            state: "available".into(),
            health: Health::Ok,
            kubernetes_namespace: None,
            pvc_name: None,
            storage_class_name: None,
        }])
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        let n = self.exports.len().max(1) as f64;
        let m = |name: &str, value: f64| MetricSample {
            name: name.into(),
            value,
            labels: Default::default(),
        };
        Ok(vec![
            m("nfs_exports_total", self.exports.len() as f64),
            m("nfs_capacity_bytes", EXPORT_CAPACITY as f64 * n),
            m("nfs_used_bytes", EXPORT_USED as f64 * n),
        ])
    }
}

// ---------------------------------------------------------------------------
// Real driver
// ---------------------------------------------------------------------------

/// One export's real (if locally mounted) or unknown capacity.
#[derive(Clone, Copy, Default)]
struct Capacity {
    total_bytes: Option<i64>,
    used_bytes: Option<i64>,
}

pub struct RealNfsDriver {
    backend_id: String,
    server: String,
    exports: Vec<String>,
}

impl RealNfsDriver {
    pub fn new(
        backend_id: impl Into<String>,
        server: impl Into<String>,
        exports: Vec<String>,
    ) -> Self {
        Self {
            backend_id: backend_id.into(),
            server: server.into(),
            exports,
        }
    }

    fn cluster_id(&self) -> String {
        format!("cls_nfs_{}", sanitize(&self.server))
    }

    /// `showmount -e <server>` — an RPC to the server's mountd, no mount performed. Returns the
    /// server's real export paths. Errors (binary missing, server unreachable, RPC refused) become
    /// `DriverError::Unreachable`, matching `RealCephDriver`'s convention of propagating rather
    /// than silently degrading.
    async fn showmount(&self) -> Result<Vec<String>, DriverError> {
        let output = tokio::process::Command::new("showmount")
            .args(["-e", &self.server])
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| DriverError::Unreachable(format!("failed to spawn `showmount`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(DriverError::Unreachable(format!(
                "showmount -e {}: {stderr}",
                self.server
            )));
        }
        // Output is "Export list for <server>:" then one "<path> <client-list>" line per export;
        // skip the header, take the first whitespace-separated token of every remaining line.
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|tok| tok.starts_with('/'))
            .map(str::to_string)
            .collect())
    }

    /// This backend's configured exports that the server actually reports via `showmount -e` —
    /// never a configured export the server doesn't have, never a real export nobody configured.
    async fn reachable_exports(&self) -> Result<Vec<String>, DriverError> {
        let real = self.showmount().await?;
        Ok(self
            .exports
            .iter()
            .filter(|e| real.iter().any(|r| r == *e))
            .cloned()
            .collect())
    }

    /// The local mount point for `<server>:<export>`, if it happens to already be mounted
    /// (searched via `/proc/mounts`) — `None` otherwise.
    async fn local_mount_point(&self, export: &str) -> Option<String> {
        let mounts = tokio::fs::read_to_string("/proc/mounts").await.ok()?;
        let source = format!("{}:{export}", self.server);
        mounts.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            let dev = parts.next()?;
            let mnt = parts.next()?;
            (dev == source).then(|| mnt.to_string())
        })
    }

    /// Real capacity via `df` on the export's local mount point, when it has one; `None` fields
    /// otherwise (NFS has no standard remote capacity query without mounting).
    async fn export_capacity(&self, export: &str) -> Capacity {
        let Some(mount_point) = self.local_mount_point(export).await else {
            return Capacity::default();
        };
        let Ok(output) = tokio::process::Command::new("df")
            .args(["-B1", "--output=size,used", &mount_point])
            .kill_on_drop(true)
            .output()
            .await
        else {
            return Capacity::default();
        };
        if !output.status.success() {
            return Capacity::default();
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let Some(data_line) = stdout.lines().nth(1) else {
            return Capacity::default();
        };
        let mut cols = data_line.split_whitespace();
        let total_bytes = cols.next().and_then(|s| s.parse::<i64>().ok());
        let used_bytes = cols.next().and_then(|s| s.parse::<i64>().ok());
        Capacity {
            total_bytes,
            used_bytes,
        }
    }

    /// Sum of every reachable export's known capacity — `None` when none of them are locally
    /// mounted (rather than reporting a misleading partial or fabricated total).
    async fn total_capacity(&self, exports: &[String]) -> (Option<i64>, Option<i64>) {
        let mut total = 0i64;
        let mut used = 0i64;
        let mut any_known = false;
        for e in exports {
            let c = self.export_capacity(e).await;
            if let (Some(t), Some(u)) = (c.total_bytes, c.used_bytes) {
                total += t;
                used += u;
                any_known = true;
            }
        }
        if any_known {
            (Some(total), Some(used))
        } else {
            (None, None)
        }
    }

    async fn cluster(&self, exports: &[String]) -> StorageCluster {
        let (raw, used) = self.total_capacity(exports).await;
        StorageCluster {
            id: self.cluster_id(),
            backend_id: self.backend_id.clone(),
            name: format!("nfs://{}", self.server),
            native_fsid: None,
            health: Health::Ok,
            raw_capacity_bytes: raw,
            used_capacity_bytes: used,
            available_capacity_bytes: match (raw, used) {
                (Some(r), Some(u)) => Some(r - u),
                _ => None,
            },
        }
    }

    async fn pool_for(&self, export: &str) -> StoragePool {
        let c = self.export_capacity(export).await;
        StoragePool {
            id: format!("pool_nfs_{}", sanitize(export)),
            cluster_id: self.cluster_id(),
            name: export.to_string(),
            kind: "nfs_export".into(),
            device_class: None,
            replica_size: None,
            used_bytes: c.used_bytes,
            max_bytes: c.total_bytes,
            health: Health::Ok,
        }
    }

    async fn pools(&self, exports: &[String]) -> Vec<StoragePool> {
        let mut out = Vec::with_capacity(exports.len());
        for e in exports {
            out.push(self.pool_for(e).await);
        }
        out
    }
}

#[async_trait]
impl StorageDriver for RealNfsDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        let exports = self.reachable_exports().await?;
        let mut volumes = Vec::new();
        for export in &exports {
            volumes.extend(self.list_volumes(export).await?);
        }
        Ok(DiscoveryResult {
            cluster: self.cluster(&exports).await,
            pools: self.pools(&exports).await,
            osds: vec![],
            volumes,
            health: self.health().await?,
        })
    }

    async fn health(&self) -> Result<StorageHealth, DriverError> {
        let exports = self.reachable_exports().await?;
        let (raw, used) = self.total_capacity(&exports).await;
        Ok(StorageHealth {
            status: Health::Ok,
            summary: format!("{} export(s) reachable on {}", exports.len(), self.server),
            raw_capacity_bytes: raw,
            used_capacity_bytes: used,
            available_capacity_bytes: match (raw, used) {
                (Some(r), Some(u)) => Some(r - u),
                _ => None,
            },
            recovering: false,
            degraded_objects: 0,
        })
    }

    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        let exports = self.reachable_exports().await?;
        Ok(self.pools(&exports).await)
    }

    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        let exports = self.reachable_exports().await?;
        if !exports.iter().any(|e| e == pool) {
            return Ok(vec![]);
        }
        let share = sanitize(pool);
        let c = self.export_capacity(pool).await;
        Ok(vec![StorageVolume {
            id: format!("vol_nfs_{share}"),
            cluster_id: Some(self.cluster_id()),
            pool_id: Some(format!("pool_nfs_{share}")),
            name: format!("{pool}/share"),
            kind: VolumeKind::Filesystem,
            backend_native_id: Some(format!("{}:{}", self.server, pool)),
            size_bytes: c.total_bytes.unwrap_or(0),
            used_bytes: c.used_bytes,
            state: "available".into(),
            health: Health::Ok,
            kubernetes_namespace: None,
            pvc_name: None,
            storage_class_name: None,
        }])
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        let exports = self.reachable_exports().await?;
        let (raw, used) = self.total_capacity(&exports).await;
        let m = |name: &str, value: f64| MetricSample {
            name: name.into(),
            value,
            labels: Default::default(),
        };
        let mut out = vec![m("nfs_exports_total", exports.len() as f64)];
        if let Some(raw) = raw {
            out.push(m("nfs_capacity_bytes", raw as f64));
        }
        if let Some(used) = used {
            out.push(m("nfs_used_bytes", used as f64));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_discover_maps_exports_to_pools_and_volumes() {
        let d = FakeNfsDriver::new(
            "bkd_nfs",
            "nfs01.example.com",
            vec!["/exports/a".into(), "/exports/b".into()],
        );
        let r = d.discover().await.unwrap();
        assert_eq!(r.cluster.backend_id, "bkd_nfs");
        assert_eq!(r.pools.len(), 2);
        assert_eq!(r.pools[0].kind, "nfs_export");
        assert_eq!(r.volumes.len(), 2);
        assert!(matches!(r.volumes[0].kind, VolumeKind::Filesystem));
        assert!(r.osds.is_empty());
        // capacity scales with export count
        assert_eq!(r.cluster.raw_capacity_bytes, Some(EXPORT_CAPACITY * 2));
    }

    #[tokio::test]
    async fn fake_unknown_pool_has_no_volumes() {
        let d = FakeNfsDriver::with_defaults("bkd_nfs");
        assert!(d.list_volumes("/nope").await.unwrap().is_empty());
    }

    /// `showmount` isn't available/won't resolve an `.invalid` host in CI, so every real-command
    /// call surfaces as `DriverError::Unreachable` — this is the correct, honest behavior (no
    /// fabricated fallback data), and is exactly what the test verifies.
    #[tokio::test]
    async fn real_unreachable_server_errors_instead_of_fabricating_data() {
        let d = RealNfsDriver::new(
            "bkd_nfs",
            "nfs01.invalid.example.invalid",
            vec!["/exports/a".into(), "/exports/b".into()],
        );
        let err = d.discover().await.unwrap_err();
        assert!(matches!(err, DriverError::Unreachable(_)), "{err:?}");
    }

    #[tokio::test]
    async fn real_list_volumes_also_verifies_reachability_first() {
        // list_volumes still tries to verify reachability (honest), so an unreachable server
        // surfaces as an error here too rather than silently returning an empty list.
        let d = RealNfsDriver::new(
            "bkd_nfs",
            "nfs01.invalid.example.invalid",
            vec!["/exports/a".into()],
        );
        let err = d.list_volumes("/nope").await.unwrap_err();
        assert!(matches!(err, DriverError::Unreachable(_)), "{err:?}");
    }

    #[test]
    fn sanitize_strips_non_alphanumerics() {
        assert_eq!(sanitize("/exports/vm-store"), "exports_vm_store");
    }
}
