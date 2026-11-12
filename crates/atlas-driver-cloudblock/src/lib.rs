// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Cloud block storage driver — EBS / Persistent Disk / Azure Disk behind the `StorageDriver`
//! trait, same shape as `atlas-driver-nfs`.
//!
//! The provider (`aws`, `gcp`, or `azure`) is the `server` field: one cluster and one
//! `StoragePool` (`kind = "cloud_block"`). Each entry in `targets` is a cloud volume id and
//! becomes a block `StorageVolume`.
//!
//! Two implementations, selected by `ATLAS_CLOUDBLOCK_DRIVER_MODE` (`fake`, the default, or `real`):
//! - [`FakeCloudBlockDriver`] invents the configured volumes with deterministic capacity — no
//!   cloud API access, safe for tests/CI/demos.
//! - [`RealCloudBlockDriver`] talks to AWS only, via `aws ec2 describe-volumes`, and only when
//!   that CLI is on `PATH`. A configured volume id the API does not return is dropped, never
//!   fabricated. Provisioned size comes from the `Size` field (GiB); used bytes stay `None`
//!   because the API does not report them. `gcp` and `azure` return
//!   [`DriverError::NotImplemented`]. A missing `aws` binary is [`DriverError::Unreachable`].
//!   Security rule (PDF §17.3): every command is an argument array, never a shell string.

use std::time::Duration;

use async_trait::async_trait;
use atlas_api_types::{
    DiscoveryResult, Health, MetricSample, StorageCluster, StorageHealth, StoragePool,
    StorageVolume, VolumeKind,
};
use atlas_driver_core::{DriverError, StorageDriver};

/// How long `aws ec2 describe-volumes` may run before the CLI is treated as unavailable.
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(30);

const GIB: i64 = 1024 * 1024 * 1024;

const GCP_NOT_IMPLEMENTED: &str = "gcp cloud block is not implemented; the real driver supports aws via `aws ec2 describe-volumes` only";
const AZURE_NOT_IMPLEMENTED: &str = "azure cloud block is not implemented; the real driver supports aws via `aws ec2 describe-volumes` only";
const PROVIDER_NOT_IMPLEMENTED: &str = "cloud block provider is not implemented (expected aws, gcp, or azure; only aws is implemented)";

/// Slugify a provider or volume id into an id-safe token.
fn sanitize(s: &str) -> String {
    s.trim_matches('/')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// One volume from `aws ec2 describe-volumes`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CloudVolume {
    id: String,
    /// Provisioned size in bytes, when the API sent `Size`.
    size_bytes: Option<i64>,
    state: String,
}

fn parse_describe_volumes(value: &serde_json::Value) -> Result<Vec<CloudVolume>, DriverError> {
    let vols = value
        .get("Volumes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            DriverError::Parse("aws ec2 describe-volumes: missing Volumes array".into())
        })?;
    let mut out = Vec::new();
    for v in vols {
        let Some(id) = v.get("VolumeId").and_then(|s| s.as_str()) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let size_bytes = match v.get("Size") {
            Some(size) => Some(
                size.as_i64()
                    .ok_or_else(|| {
                        DriverError::Parse(format!(
                            "aws ec2 describe-volumes: Size for {id} is not an integer"
                        ))
                    })?
                    .saturating_mul(GIB),
            ),
            None => None,
        };
        let state = v
            .get("State")
            .and_then(|s| s.as_str())
            .unwrap_or("available")
            .to_string();
        out.push(CloudVolume {
            id: id.to_string(),
            size_bytes,
            state,
        });
    }
    Ok(out)
}

/// Keep configured ids the API actually returned, in the configured order. Ids the API did not
/// return are dropped.
fn filter_configured(discovered: &[CloudVolume], targets: &[String]) -> Vec<CloudVolume> {
    targets
        .iter()
        .filter_map(|id| discovered.iter().find(|v| v.id == id.trim()).cloned())
        .collect()
}

fn health_for_state(state: &str) -> Health {
    match state {
        "available" | "in-use" => Health::Ok,
        "creating" | "deleting" => Health::Warn,
        "error" | "deleted" => Health::Critical,
        _ => Health::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Fake (fixture) driver
// ---------------------------------------------------------------------------

/// Per-volume capacity fixture (bytes).
const VOLUME_CAPACITY: i64 = 1_000_000_000_000; // 1 TB
const VOLUME_USED: i64 = 250_000_000_000; // 25% used

pub struct FakeCloudBlockDriver {
    backend_id: String,
    /// `aws`, `gcp`, or `azure` (any string is accepted; the fixture does not call a cloud API).
    provider: String,
    /// Volume ids the fixture invents.
    targets: Vec<String>,
}

impl FakeCloudBlockDriver {
    /// Build from a provider (`server`) and the volume ids to invent.
    pub fn new(
        backend_id: impl Into<String>,
        provider: impl Into<String>,
        targets: Vec<String>,
    ) -> Self {
        Self {
            backend_id: backend_id.into(),
            provider: provider.into().trim().to_ascii_lowercase(),
            targets,
        }
    }

    /// Demo fixture used when cloud block is enabled without an explicit provider/volumes.
    pub fn with_defaults(backend_id: impl Into<String>) -> Self {
        Self::new(
            backend_id,
            "aws",
            vec!["vol-atlas00000000001".into(), "vol-atlas00000000002".into()],
        )
    }

    fn cluster_id(&self) -> String {
        format!("cls_cloudblock_{}", sanitize(&self.provider))
    }

    fn pool_id(&self) -> String {
        format!("pool_cloudblock_{}", sanitize(&self.provider))
    }

    fn totals(&self) -> (i64, i64) {
        let n = self.targets.len() as i64;
        (VOLUME_CAPACITY * n, VOLUME_USED * n)
    }

    fn cluster(&self) -> StorageCluster {
        let (raw, used) = self.totals();
        StorageCluster {
            id: self.cluster_id(),
            backend_id: self.backend_id.clone(),
            name: format!("cloudblock://{}", self.provider),
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
            name: self.provider.clone(),
            kind: "cloud_block".into(),
            device_class: None,
            replica_size: None,
            used_bytes: Some(used),
            max_bytes: Some(raw),
            health: Health::Ok,
        }
    }

    fn volume(&self, id: &str) -> StorageVolume {
        StorageVolume {
            id: format!("vol_cloudblock_{}", sanitize(id)),
            cluster_id: Some(self.cluster_id()),
            pool_id: Some(self.pool_id()),
            name: id.to_string(),
            kind: VolumeKind::Block,
            backend_native_id: Some(format!("{}:{}", self.provider, id)),
            size_bytes: VOLUME_CAPACITY,
            used_bytes: Some(VOLUME_USED),
            state: "available".into(),
            health: Health::Ok,
            kubernetes_namespace: None,
            pvc_name: None,
            storage_class_name: None,
        }
    }
}

#[async_trait]
impl StorageDriver for FakeCloudBlockDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        let volumes = self.list_volumes(&self.provider).await?;
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
            summary: format!("{} volume(s) on {}", self.targets.len(), self.provider),
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

    /// Invented block volumes for this provider. `pool` is the provider (`StoragePool.name`).
    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        if !pool.eq_ignore_ascii_case(&self.provider) {
            return Ok(vec![]);
        }
        Ok(self.targets.iter().map(|id| self.volume(id)).collect())
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        let (raw, used) = self.totals();
        let m = |name: &str, value: f64| MetricSample {
            name: name.into(),
            value,
            labels: Default::default(),
        };
        Ok(vec![
            m("cloudblock_volumes_total", self.targets.len() as f64),
            m("cloudblock_capacity_bytes", raw as f64),
            m("cloudblock_used_bytes", used as f64),
        ])
    }
}

// ---------------------------------------------------------------------------
// Real driver
// ---------------------------------------------------------------------------

pub struct RealCloudBlockDriver {
    backend_id: String,
    provider: String,
    targets: Vec<String>,
}

impl RealCloudBlockDriver {
    pub fn new(
        backend_id: impl Into<String>,
        provider: impl Into<String>,
        targets: Vec<String>,
    ) -> Self {
        Self {
            backend_id: backend_id.into(),
            provider: provider.into().trim().to_ascii_lowercase(),
            targets,
        }
    }

    fn provider_gate(&self) -> Result<(), DriverError> {
        match self.provider.as_str() {
            "aws" => Ok(()),
            "gcp" => Err(DriverError::NotImplemented(GCP_NOT_IMPLEMENTED)),
            "azure" => Err(DriverError::NotImplemented(AZURE_NOT_IMPLEMENTED)),
            _ => Err(DriverError::NotImplemented(PROVIDER_NOT_IMPLEMENTED)),
        }
    }

    fn cluster_id(&self) -> String {
        format!("cls_cloudblock_{}", sanitize(&self.provider))
    }

    fn pool_id(&self) -> String {
        format!("pool_cloudblock_{}", sanitize(&self.provider))
    }

    /// `aws ec2 describe-volumes --output json`, optionally filtered to the configured ids.
    /// Only call after [`Self::provider_gate`] has accepted `aws`.
    async fn describe_volumes(&self) -> Result<Vec<CloudVolume>, DriverError> {
        let mut args = vec![
            "ec2".to_string(),
            "describe-volumes".to_string(),
            "--output".to_string(),
            "json".to_string(),
        ];
        if !self.targets.is_empty() {
            args.push("--filters".into());
            args.push(format!(
                "Name=volume-id,Values={}",
                self.targets
                    .iter()
                    .map(|id| id.trim())
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        let command = tokio::process::Command::new("aws")
            .args(&args)
            .kill_on_drop(true)
            .output();
        let output = match tokio::time::timeout(DESCRIBE_TIMEOUT, command).await {
            Ok(result) => result.map_err(|e| {
                DriverError::Unreachable(format!(
                    "aws CLI is not available (install the AWS CLI v2 and configure credentials): {e}"
                ))
            })?,
            Err(_) => {
                return Err(DriverError::Unreachable(
                    "aws ec2 describe-volumes timed out; aws CLI unavailable or the API did not answer"
                        .into(),
                ));
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                format!("exit status {}", output.status)
            } else {
                stderr
            };
            return Err(DriverError::Backend(format!(
                "aws ec2 describe-volumes failed: {detail}"
            )));
        }
        let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| DriverError::Parse(format!("aws ec2 describe-volumes json: {e}")))?;
        let discovered = parse_describe_volumes(&parsed)?;
        Ok(filter_configured(&discovered, &self.targets))
    }

    fn capacity_of(volumes: &[CloudVolume]) -> Option<i64> {
        let mut total = 0i64;
        let mut any = false;
        for v in volumes {
            if let Some(size) = v.size_bytes {
                total += size;
                any = true;
            }
        }
        any.then_some(total)
    }

    fn cluster(&self, volumes: &[CloudVolume]) -> StorageCluster {
        let raw = Self::capacity_of(volumes);
        StorageCluster {
            id: self.cluster_id(),
            backend_id: self.backend_id.clone(),
            name: format!("cloudblock://{}", self.provider),
            native_fsid: None,
            health: Health::Ok,
            raw_capacity_bytes: raw,
            // describe-volumes reports provisioned Size, not bytes used.
            used_capacity_bytes: None,
            available_capacity_bytes: None,
        }
    }

    fn pool(&self, volumes: &[CloudVolume]) -> StoragePool {
        StoragePool {
            id: self.pool_id(),
            cluster_id: self.cluster_id(),
            name: self.provider.clone(),
            kind: "cloud_block".into(),
            device_class: None,
            replica_size: None,
            used_bytes: None,
            max_bytes: Self::capacity_of(volumes),
            health: Health::Ok,
        }
    }

    fn volume(&self, volume: &CloudVolume) -> StorageVolume {
        StorageVolume {
            id: format!("vol_cloudblock_{}", sanitize(&volume.id)),
            cluster_id: Some(self.cluster_id()),
            pool_id: Some(self.pool_id()),
            name: volume.id.clone(),
            kind: VolumeKind::Block,
            backend_native_id: Some(format!("{}:{}", self.provider, volume.id)),
            size_bytes: volume.size_bytes.unwrap_or(0),
            used_bytes: None,
            state: volume.state.clone(),
            health: health_for_state(&volume.state),
            kubernetes_namespace: None,
            pvc_name: None,
            storage_class_name: None,
        }
    }
}

#[async_trait]
impl StorageDriver for RealCloudBlockDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        self.provider_gate()?;
        let volumes = self.describe_volumes().await?;
        let listed = volumes.iter().map(|v| self.volume(v)).collect();
        Ok(DiscoveryResult {
            cluster: self.cluster(&volumes),
            pools: vec![self.pool(&volumes)],
            osds: vec![],
            volumes: listed,
            health: self.health().await?,
        })
    }

    async fn health(&self) -> Result<StorageHealth, DriverError> {
        self.provider_gate()?;
        let volumes = self.describe_volumes().await?;
        Ok(StorageHealth {
            status: Health::Ok,
            summary: format!("{} volume(s) on {}", volumes.len(), self.provider),
            raw_capacity_bytes: Self::capacity_of(&volumes),
            used_capacity_bytes: None,
            available_capacity_bytes: None,
            recovering: false,
            degraded_objects: 0,
        })
    }

    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        self.provider_gate()?;
        let volumes = self.describe_volumes().await?;
        Ok(vec![self.pool(&volumes)])
    }

    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        self.provider_gate()?;
        let volumes = self.describe_volumes().await?;
        if !pool.eq_ignore_ascii_case(&self.provider) {
            return Ok(vec![]);
        }
        Ok(volumes.iter().map(|v| self.volume(v)).collect())
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        self.provider_gate()?;
        let volumes = self.describe_volumes().await?;
        let mut out = vec![MetricSample {
            name: "cloudblock_volumes_total".into(),
            value: volumes.len() as f64,
            labels: Default::default(),
        }];
        if let Some(raw) = Self::capacity_of(&volumes) {
            out.push(MetricSample {
                name: "cloudblock_capacity_bytes".into(),
                value: raw as f64,
                labels: Default::default(),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_discover_invents_block_volumes() {
        let d =
            FakeCloudBlockDriver::new("bkd_cloud", "aws", vec!["vol-aaa".into(), "vol-bbb".into()]);
        let r = d.discover().await.unwrap();
        assert_eq!(r.cluster.backend_id, "bkd_cloud");
        assert_eq!(r.cluster.name, "cloudblock://aws");
        assert_eq!(r.pools.len(), 1);
        assert_eq!(r.pools[0].kind, "cloud_block");
        assert_eq!(r.pools[0].name, "aws");
        assert_eq!(r.volumes.len(), 2);
        assert!(matches!(r.volumes[0].kind, VolumeKind::Block));
        assert_eq!(r.volumes[0].name, "vol-aaa");
        assert_eq!(r.volumes[0].size_bytes, VOLUME_CAPACITY);
        assert!(r.osds.is_empty());
        assert_eq!(r.cluster.raw_capacity_bytes, Some(VOLUME_CAPACITY * 2));
        assert_eq!(d.list_volumes("aws").await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn fake_unknown_pool_has_no_volumes() {
        let d = FakeCloudBlockDriver::with_defaults("bkd_cloud");
        assert!(d.list_volumes("gcp").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn real_gcp_and_azure_return_not_implemented() {
        for provider in ["gcp", "azure", "GCP"] {
            let d = RealCloudBlockDriver::new("bkd_cloud", provider, vec!["vol-1".into()]);
            let err = d.discover().await.unwrap_err();
            assert!(
                matches!(err, DriverError::NotImplemented(_)),
                "{provider}: {err:?}"
            );
            let message = err.to_string();
            assert!(message.contains("not implemented"), "{provider}: {message}");
        }
    }

    #[test]
    fn describe_volumes_keeps_configured_ids_and_real_sizes_only() {
        let body = serde_json::json!({
            "Volumes": [
                {"VolumeId": "vol-aaa", "Size": 8, "State": "available"},
                {"VolumeId": "vol-bbb", "Size": 100, "State": "in-use"},
                {"VolumeId": "vol-other", "Size": 1, "State": "available"}
            ]
        });
        let all = parse_describe_volumes(&body).unwrap();
        let kept = filter_configured(&all, &["vol-bbb".into(), "vol-missing".into()]);
        assert_eq!(
            kept.len(),
            1,
            "a volume the API did not return must be dropped"
        );
        assert_eq!(kept[0].id, "vol-bbb");
        assert_eq!(kept[0].size_bytes, Some(100 * GIB));
        assert_eq!(kept[0].state, "in-use");
        assert!(kept.iter().all(|v| v.id != "vol-missing"));
    }

    #[test]
    fn describe_volumes_does_not_invent_a_size_when_size_is_absent() {
        let body = serde_json::json!({
            "Volumes": [{"VolumeId": "vol-nosize", "State": "available"}]
        });
        let all = parse_describe_volumes(&body).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].size_bytes, None);
        assert_eq!(RealCloudBlockDriver::capacity_of(&all), None);
    }

    #[test]
    fn sanitize_strips_non_alphanumerics() {
        assert_eq!(sanitize("vol-atlas0001"), "vol_atlas0001");
    }
}
