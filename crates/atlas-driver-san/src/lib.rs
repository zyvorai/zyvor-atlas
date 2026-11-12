// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! SAN (iSCSI) storage driver — a block backend behind the `StorageDriver` trait, same shape as
//! `atlas-driver-nfs`.
//!
//! One portal is one cluster and one `StoragePool` (`kind = "san_portal"`). Each configured
//! LUN/target path is a block `StorageVolume`.
//!
//! Two implementations, selected by `ATLAS_SAN_DRIVER_MODE` (`fake`, the default, or `real`):
//! - [`FakeSanDriver`] reports deterministic capacity fixtures — no network access at all, safe
//!   for tests/CI/demos without a real iSCSI portal.
//! - [`RealSanDriver`] shells out to `iscsiadm -m discovery -t st -p <portal>` (SendTargets, no
//!   login). A configured target the portal does not advertise is dropped, never fabricated.
//!   Discovery does not report LUN size, so capacity stays `None` rather than a made-up number.
//!   If `iscsiadm` is missing, times out, or the portal does not answer, the call is a
//!   [`DriverError`] — never a fixture fallback. Security rule (PDF §17.3): every command is an
//!   argument array, never a shell string.

use std::time::Duration;

use async_trait::async_trait;
use atlas_api_types::{
    DiscoveryResult, Health, MetricSample, StorageCluster, StorageHealth, StoragePool,
    StorageVolume, VolumeKind,
};
use atlas_driver_core::{DriverError, StorageDriver};

/// How long SendTargets discovery may run before the portal is treated as unavailable.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Slugify a portal or target into an id-safe token.
fn sanitize(s: &str) -> String {
    s.trim_matches('/')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// IQNs (second column) from `iscsiadm -m discovery -t st` stdout.
fn parse_iscsiadm_discovery(stdout: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        let Some(_portal) = parts.next() else {
            continue;
        };
        let Some(target) = parts.next() else {
            continue;
        };
        // The portal column is `host:port,tpgt`. The target column is an IQN, EUI, or NAA name.
        // Anything else (a header, a wrapped word) is not a LUN and must not be inventoried.
        let name = target.to_ascii_lowercase();
        let is_target =
            name.starts_with("iqn.") || name.starts_with("eui.") || name.starts_with("naa.");
        if !is_target || out.iter().any(|seen| seen == target) {
            continue;
        }
        out.push(target.to_string());
    }
    out
}

fn target_advertised(configured: &str, discovered: &[String]) -> bool {
    let configured = configured.trim();
    discovered.iter().any(|iqn| iqn == configured)
}

// ---------------------------------------------------------------------------
// Fake (fixture) driver
// ---------------------------------------------------------------------------

/// Per-target capacity fixture (bytes).
const TARGET_CAPACITY: i64 = 2_000_000_000_000; // 2 TB
const TARGET_USED: i64 = 500_000_000_000; // 25% used

pub struct FakeSanDriver {
    backend_id: String,
    /// iSCSI portal (`host` or `host:port`).
    portal: String,
    /// LUN/target paths (IQNs) the fixture reports.
    targets: Vec<String>,
}

impl FakeSanDriver {
    /// Build from an iSCSI portal and its LUN/target paths.
    pub fn new(
        backend_id: impl Into<String>,
        portal: impl Into<String>,
        targets: Vec<String>,
    ) -> Self {
        Self {
            backend_id: backend_id.into(),
            portal: portal.into().trim().to_string(),
            targets,
        }
    }

    /// Demo fixture used when SAN is enabled without an explicit portal/targets.
    pub fn with_defaults(backend_id: impl Into<String>) -> Self {
        Self::new(
            backend_id,
            "san01.zyvor.lab",
            vec![
                "iqn.2026-01.com.zyvor:lun0".into(),
                "iqn.2026-01.com.zyvor:lun1".into(),
            ],
        )
    }

    fn cluster_id(&self) -> String {
        format!("cls_san_{}", sanitize(&self.portal))
    }

    fn pool_id(&self) -> String {
        format!("pool_san_{}", sanitize(&self.portal))
    }

    fn totals(&self) -> (i64, i64) {
        let n = self.targets.len() as i64;
        (TARGET_CAPACITY * n, TARGET_USED * n)
    }

    fn cluster(&self) -> StorageCluster {
        let (raw, used) = self.totals();
        StorageCluster {
            id: self.cluster_id(),
            backend_id: self.backend_id.clone(),
            name: format!("san://{}", self.portal),
            native_fsid: None,
            health: Health::Ok,
            raw_capacity_bytes: Some(raw),
            used_capacity_bytes: Some(used),
            available_capacity_bytes: Some(raw - used),
        }
    }

    fn pool(&self) -> StoragePool {
        let (raw, used) = self.totals();
        StoragePool {
            id: self.pool_id(),
            cluster_id: self.cluster_id(),
            name: self.portal.clone(),
            kind: "san_portal".into(),
            device_class: None,
            replica_size: None,
            used_bytes: Some(used),
            max_bytes: Some(raw),
            health: Health::Ok,
        }
    }

    fn volume(&self, target: &str) -> StorageVolume {
        StorageVolume {
            id: format!("vol_san_{}", sanitize(target)),
            cluster_id: Some(self.cluster_id()),
            pool_id: Some(self.pool_id()),
            name: target.to_string(),
            kind: VolumeKind::Block,
            backend_native_id: Some(format!("{}:{}", self.portal, target)),
            size_bytes: TARGET_CAPACITY,
            used_bytes: Some(TARGET_USED),
            state: "available".into(),
            health: Health::Ok,
            kubernetes_namespace: None,
            pvc_name: None,
            storage_class_name: None,
        }
    }
}

#[async_trait]
impl StorageDriver for FakeSanDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        let volumes = self.list_volumes(&self.portal).await?;
        Ok(DiscoveryResult {
            cluster: self.cluster(),
            pools: vec![self.pool()],
            osds: vec![],
            volumes,
            health: self.health().await?,
        })
    }

    async fn health(&self) -> Result<StorageHealth, DriverError> {
        let (raw, used) = self.totals();
        Ok(StorageHealth {
            status: Health::Ok,
            summary: format!("{} target(s) on portal {}", self.targets.len(), self.portal),
            raw_capacity_bytes: Some(raw),
            used_capacity_bytes: Some(used),
            available_capacity_bytes: Some(raw - used),
            recovering: false,
            degraded_objects: 0,
        })
    }

    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        Ok(vec![self.pool()])
    }

    /// Block LUNs advertised by this portal. `pool` is the portal (matches `StoragePool.name`).
    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        if pool != self.portal {
            return Ok(vec![]);
        }
        Ok(self.targets.iter().map(|t| self.volume(t)).collect())
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        let (raw, used) = self.totals();
        let m = |name: &str, value: f64| MetricSample {
            name: name.into(),
            value,
            labels: Default::default(),
        };
        Ok(vec![
            m("san_targets_total", self.targets.len() as f64),
            m("san_capacity_bytes", raw as f64),
            m("san_used_bytes", used as f64),
        ])
    }
}

// ---------------------------------------------------------------------------
// Real driver
// ---------------------------------------------------------------------------

pub struct RealSanDriver {
    backend_id: String,
    portal: String,
    targets: Vec<String>,
}

impl RealSanDriver {
    pub fn new(
        backend_id: impl Into<String>,
        portal: impl Into<String>,
        targets: Vec<String>,
    ) -> Self {
        Self {
            backend_id: backend_id.into(),
            portal: portal.into().trim().to_string(),
            targets,
        }
    }

    fn cluster_id(&self) -> String {
        format!("cls_san_{}", sanitize(&self.portal))
    }

    fn pool_id(&self) -> String {
        format!("pool_san_{}", sanitize(&self.portal))
    }

    /// `iscsiadm -m discovery -t st -p <portal>`. Missing binary, timeout, and a portal that does
    /// not answer are all [`DriverError::Unreachable`] — capacity is never filled in instead.
    async fn iscsi_discovery(&self) -> Result<Vec<String>, DriverError> {
        let command = tokio::process::Command::new("iscsiadm")
            .args(["-m", "discovery", "-t", "st", "-p", &self.portal])
            .kill_on_drop(true)
            .output();
        let output = match tokio::time::timeout(DISCOVERY_TIMEOUT, command).await {
            Ok(result) => result.map_err(|e| {
                DriverError::Unreachable(format!(
                    "iscsiadm is not available (install open-iscsi): {e}"
                ))
            })?,
            Err(_) => {
                return Err(DriverError::Unreachable(format!(
                    "iscsiadm -m discovery -t st -p {} timed out; iscsiadm unavailable or portal unreachable",
                    self.portal
                )));
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                format!("exit status {}", output.status)
            } else {
                stderr
            };
            return Err(DriverError::Unreachable(format!(
                "iscsiadm -m discovery -t st -p {} failed: {detail}",
                self.portal
            )));
        }
        Ok(parse_iscsiadm_discovery(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }

    /// Configured targets the portal actually advertised. Unconfigured IQNs are ignored; a
    /// configured target that was not advertised is dropped, never invented.
    async fn reachable_targets(&self) -> Result<Vec<String>, DriverError> {
        let discovered = self.iscsi_discovery().await?;
        Ok(self
            .targets
            .iter()
            .filter(|t| target_advertised(t, &discovered))
            .cloned()
            .collect())
    }

    fn cluster(&self) -> StorageCluster {
        StorageCluster {
            id: self.cluster_id(),
            backend_id: self.backend_id.clone(),
            name: format!("san://{}", self.portal),
            native_fsid: None,
            health: Health::Ok,
            // SendTargets does not report LUN capacity. Leave it unknown.
            raw_capacity_bytes: None,
            used_capacity_bytes: None,
            available_capacity_bytes: None,
        }
    }

    fn pool(&self) -> StoragePool {
        StoragePool {
            id: self.pool_id(),
            cluster_id: self.cluster_id(),
            name: self.portal.clone(),
            kind: "san_portal".into(),
            device_class: None,
            replica_size: None,
            used_bytes: None,
            max_bytes: None,
            health: Health::Ok,
        }
    }

    fn volume(&self, target: &str) -> StorageVolume {
        StorageVolume {
            id: format!("vol_san_{}", sanitize(target)),
            cluster_id: Some(self.cluster_id()),
            pool_id: Some(self.pool_id()),
            name: target.to_string(),
            kind: VolumeKind::Block,
            backend_native_id: Some(format!("{}:{}", self.portal, target)),
            // `size_bytes` is not optional. `0` here means "not reported", matching the `None`
            // capacity on the pool — it is not a measured empty LUN.
            size_bytes: 0,
            used_bytes: None,
            state: "available".into(),
            health: Health::Ok,
            kubernetes_namespace: None,
            pvc_name: None,
            storage_class_name: None,
        }
    }
}

#[async_trait]
impl StorageDriver for RealSanDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        let targets = self.reachable_targets().await?;
        let volumes = targets.iter().map(|t| self.volume(t)).collect();
        Ok(DiscoveryResult {
            cluster: self.cluster(),
            pools: vec![self.pool()],
            osds: vec![],
            volumes,
            health: self.health().await?,
        })
    }

    async fn health(&self) -> Result<StorageHealth, DriverError> {
        let targets = self.reachable_targets().await?;
        Ok(StorageHealth {
            status: Health::Ok,
            summary: format!("{} target(s) on portal {}", targets.len(), self.portal),
            raw_capacity_bytes: None,
            used_capacity_bytes: None,
            available_capacity_bytes: None,
            recovering: false,
            degraded_objects: 0,
        })
    }

    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        // Confirm the portal answers before reporting a pool. Capacity stays unknown.
        let _targets = self.reachable_targets().await?;
        Ok(vec![self.pool()])
    }

    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        let targets = self.reachable_targets().await?;
        if pool != self.portal {
            return Ok(vec![]);
        }
        Ok(targets.iter().map(|t| self.volume(t)).collect())
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        let targets = self.reachable_targets().await?;
        // No capacity series: discovery did not report one, and a zero would be a fabrication.
        Ok(vec![MetricSample {
            name: "san_targets_total".into(),
            value: targets.len() as f64,
            labels: Default::default(),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_discover_maps_portal_to_pool_and_targets_to_block_volumes() {
        let d = FakeSanDriver::new(
            "bkd_san",
            "10.1.0.5:3260",
            vec![
                "iqn.2026-01.com.zyvor:lun0".into(),
                "iqn.2026-01.com.zyvor:lun1".into(),
            ],
        );
        let r = d.discover().await.unwrap();
        assert_eq!(r.cluster.backend_id, "bkd_san");
        assert_eq!(r.cluster.name, "san://10.1.0.5:3260");
        assert_eq!(r.pools.len(), 1);
        assert_eq!(r.pools[0].kind, "san_portal");
        assert_eq!(r.pools[0].name, "10.1.0.5:3260");
        assert_eq!(r.volumes.len(), 2);
        assert!(matches!(r.volumes[0].kind, VolumeKind::Block));
        assert_eq!(r.volumes[0].name, "iqn.2026-01.com.zyvor:lun0");
        assert!(r.osds.is_empty());
        assert_eq!(r.cluster.raw_capacity_bytes, Some(TARGET_CAPACITY * 2));
        assert_eq!(d.list_volumes("10.1.0.5:3260").await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn fake_unknown_pool_has_no_volumes() {
        let d = FakeSanDriver::with_defaults("bkd_san");
        assert!(d.list_volumes("other-portal").await.unwrap().is_empty());
    }

    #[test]
    fn parse_iscsiadm_discovery_keeps_target_column() {
        let stdout = "\
10.1.0.5:3260,1 iqn.2026-01.com.zyvor:lun0
10.1.0.5:3260,1 iqn.2026-01.com.zyvor:lun1
Export list for nobody
";
        let iqns = parse_iscsiadm_discovery(stdout);
        assert_eq!(
            iqns,
            vec![
                "iqn.2026-01.com.zyvor:lun0".to_string(),
                "iqn.2026-01.com.zyvor:lun1".to_string(),
            ]
        );
        assert!(target_advertised("iqn.2026-01.com.zyvor:lun0", &iqns));
        assert!(!target_advertised("iqn.2026-01.com.zyvor:missing", &iqns));
    }

    /// `iscsiadm` is absent or will not answer an `.invalid` portal here. Either way the driver
    /// returns [`DriverError::Unreachable`] and does not invent capacity.
    #[tokio::test]
    async fn real_unavailable_portal_errors_instead_of_fabricating_capacity() {
        let d = RealSanDriver::new(
            "bkd_san",
            "san01.invalid.example.invalid",
            vec!["iqn.2026-01.com.zyvor:lun0".into()],
        );
        let err = d.discover().await.unwrap_err();
        assert!(matches!(err, DriverError::Unreachable(_)), "{err:?}");
        let message = err.to_string();
        assert!(
            message.contains("iscsiadm"),
            "error should name iscsiadm, got {message}"
        );
    }

    #[test]
    fn real_reports_no_capacity_without_a_measurement() {
        let d = RealSanDriver::new(
            "bkd_san",
            "10.1.0.5",
            vec!["iqn.2026-01.com.zyvor:lun0".into()],
        );
        let cluster = d.cluster();
        assert_eq!(cluster.raw_capacity_bytes, None);
        assert_eq!(cluster.used_capacity_bytes, None);
        assert_eq!(cluster.available_capacity_bytes, None);
        let pool = d.pool();
        assert_eq!(pool.kind, "san_portal");
        assert_eq!(pool.max_bytes, None);
        let volume = d.volume("iqn.2026-01.com.zyvor:lun0");
        assert!(matches!(volume.kind, VolumeKind::Block));
        assert_eq!(volume.size_bytes, 0);
        assert_eq!(volume.used_bytes, None);
    }

    #[test]
    fn sanitize_strips_non_alphanumerics() {
        assert_eq!(
            sanitize("iqn.2026-01.com.zyvor:lun0"),
            "iqn_2026_01_com_zyvor_lun0"
        );
    }
}
