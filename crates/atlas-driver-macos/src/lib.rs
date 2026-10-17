// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! macOS storage driver: a Mac's own disks, internal and external, behind the `StorageDriver` trait.
//!
//! Each **APFS container** is a `StoragePool` (`kind = "apfs_container"`) and each APFS volume a
//! filesystem `StorageVolume`; a partition with another filesystem (HFS+, exFAT, FAT) is a pool of
//! kind `partition` holding one volume. Capacity, quota, reserve, mount point and SMART health come
//! from `diskutil`'s property lists (see [`parse`]).
//!
//! Two implementations, selected by `ATLAS_MACOS_DRIVER_MODE` (`fake`, the default, or `real`) like
//! the ZFS driver:
//! - [`FakeMacosDriver`] maps recorded plists (a built-in internal disk plus a USB SSD, an exFAT card
//!   and a disk image): no command execution at all.
//! - [`RealMacosDriver`] runs `diskutil` **locally**, on the Mac the gateway (or Mac agent) runs on.
//!   A Mac cannot be queried remotely by this driver. If `diskutil` is missing it returns
//!   `Unreachable`; it never fabricates data. Commands are argument arrays, never shell strings.
//!
//! Discovery is read-only. The [`ops`] module adds guarded lifecycle operations. Mounted disk images are not reported unless `allow_disk_images` is set.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use atlas_api_types::{
    DiscoveryResult, MetricSample, StorageHealth, StoragePool, StorageVolume,
};
use atlas_driver_core::{DriverError, StorageDriver};

pub mod ops;
pub mod parse;
pub use ops::MacosOps;
pub use parse::{
    cluster_id, map_layout, parse_info, parse_layout, whole_disk_of, ApfsContainer, ApfsVolume,
    DiskInfo, Layout, Mapped, Partition,
};

/// The `ATLAS_MACOS_*` environment, read the same way by the gateway and by job dispatch.
#[derive(Debug, Clone)]
pub struct EnvSettings {
    pub enabled: bool,
    /// `ATLAS_MACOS_DRIVER_MODE`: anything but `real` is fake.
    pub fake: bool,
    pub host: String,
    pub allow_disk_images: bool,
}

impl EnvSettings {
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let on = |k: &str| var(k).is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
        let fake = var("ATLAS_MACOS_DRIVER_MODE")
            .map(|m| m.trim().to_lowercase() != "real")
            .unwrap_or(true);
        let default_host = if fake { "mac01.zyvor.lab" } else { "localhost" };
        Self {
            enabled: on("ATLAS_MACOS_ENABLE"),
            fake,
            host: var("ATLAS_MACOS_HOST").unwrap_or_else(|| default_host.into()),
            allow_disk_images: on("ATLAS_MACOS_ALLOW_DISK_IMAGES"),
        }
    }

    pub fn ops(&self) -> MacosOps {
        if self.fake {
            MacosOps::fake(self.allow_disk_images)
        } else {
            MacosOps::real(self.allow_disk_images)
        }
    }

    pub fn driver(&self, backend_id: &str) -> std::sync::Arc<dyn StorageDriver> {
        if self.fake {
            std::sync::Arc::new(
                FakeMacosDriver::new(backend_id, self.host.clone())
                    .allow_disk_images(self.allow_disk_images),
            )
        } else {
            std::sync::Arc::new(
                RealMacosDriver::new(backend_id, self.host.clone())
                    .allow_disk_images(self.allow_disk_images),
            )
        }
    }
}

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

fn metrics_for(m: &Mapped) -> Vec<MetricSample> {
    let d = &m.discovery;
    let sample = |name: &str, value: f64| MetricSample {
        name: name.into(),
        value,
        labels: Default::default(),
    };
    let mut out = vec![
        sample(
            "macos_containers_total",
            d.pools.iter().filter(|p| p.kind == "apfs_container").count() as f64,
        ),
        sample("macos_volumes_total", d.volumes.len() as f64),
        sample("macos_smart_failing", m.smart_failing as f64),
    ];
    if let (Some(raw), Some(used)) = (d.cluster.raw_capacity_bytes, d.cluster.used_capacity_bytes) {
        out.push(sample("macos_capacity_bytes", raw as f64));
        out.push(sample("macos_used_bytes", used as f64));
    }
    out
}

/// Pools matching `pool` by name (`disk3`) or id.
fn volumes_of(d: &DiscoveryResult, pool: &str) -> Vec<StorageVolume> {
    let Some(p) = d.pools.iter().find(|p| p.name == pool || p.id == pool) else {
        return vec![];
    };
    d.volumes
        .iter()
        .filter(|v| v.pool_id.as_deref() == Some(p.id.as_str()))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Fake (fixture) driver
// ---------------------------------------------------------------------------

pub struct FakeMacosDriver {
    backend_id: String,
    host: String,
    allow_disk_images: bool,
}

impl FakeMacosDriver {
    pub fn new(backend_id: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            backend_id: backend_id.into(),
            host: host.into(),
            allow_disk_images: false,
        }
    }

    pub fn with_defaults(backend_id: impl Into<String>) -> Self {
        Self::new(backend_id, "mac01.zyvor.lab")
    }

    pub fn allow_disk_images(mut self, allow: bool) -> Self {
        self.allow_disk_images = allow;
        self
    }

    fn mapped(&self) -> Result<Mapped, DriverError> {
        let (layout, infos) = fixture_state()?;
        Ok(map_layout(
            &self.backend_id,
            &self.host,
            &layout,
            &infos,
            self.allow_disk_images,
        ))
    }
}

/// The recorded layout and per-disk info the fake driver and fake lifecycle operations use.
pub(crate) fn fixture_state() -> Result<(Layout, HashMap<String, DiskInfo>), DriverError> {
    let layout = parse_layout(
        include_bytes!("../tests/fixtures/list.plist"),
        include_bytes!("../tests/fixtures/apfs.plist"),
    )?;
    let mut infos = HashMap::new();
    for (disk, bytes) in [
        ("disk0", &include_bytes!("../tests/fixtures/info_disk0.plist")[..]),
        ("disk6", &include_bytes!("../tests/fixtures/info_disk6.plist")[..]),
        ("disk8", &include_bytes!("../tests/fixtures/info_disk8.plist")[..]),
        ("disk9", &include_bytes!("../tests/fixtures/info_disk9.plist")[..]),
    ] {
        infos.insert(disk.to_string(), parse_info(bytes)?);
    }
    Ok((layout, infos))
}

#[async_trait]
impl StorageDriver for FakeMacosDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    fn is_fixture(&self) -> bool {
        true
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        Ok(self.mapped()?.discovery)
    }

    async fn health(&self) -> Result<StorageHealth, DriverError> {
        Ok(self.mapped()?.discovery.health)
    }

    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        Ok(self.mapped()?.discovery.pools)
    }

    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        Ok(volumes_of(&self.mapped()?.discovery, pool))
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        Ok(metrics_for(&self.mapped()?))
    }
}

// ---------------------------------------------------------------------------
// Real driver
// ---------------------------------------------------------------------------

pub struct RealMacosDriver {
    backend_id: String,
    host: String,
    allow_disk_images: bool,
}

/// Run `diskutil <args>` and return stdout. Spawn failure (no `diskutil`, i.e. not a Mac) is
/// `Unreachable`; a non-zero exit or a hang past the timeout is `Backend`.
async fn diskutil(args: &[&str]) -> Result<Vec<u8>, DriverError> {
    let mut cmd = tokio::process::Command::new("diskutil");
    cmd.args(args).kill_on_drop(true);
    let output = tokio::time::timeout(COMMAND_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            DriverError::Backend(format!(
                "diskutil {}: no answer within {}s",
                args.join(" "),
                COMMAND_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `diskutil`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!(
            "diskutil {}: {stderr}",
            args.join(" ")
        )));
    }
    Ok(output.stdout)
}

impl RealMacosDriver {
    pub fn new(backend_id: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            backend_id: backend_id.into(),
            host: host.into(),
            allow_disk_images: false,
        }
    }

    /// Report mounted disk images too. Only for testing destructive operations on throwaway images.
    pub fn allow_disk_images(mut self, allow: bool) -> Self {
        self.allow_disk_images = allow;
        self
    }

    async fn mapped(&self) -> Result<Mapped, DriverError> {
        let list = diskutil(&["list", "-plist"]).await?;
        let apfs = diskutil(&["apfs", "list", "-plist"]).await?;
        let layout = parse_layout(&list, &apfs)?;

        // `diskutil info` for every whole disk that holds a container or a filesystem partition.
        let mut wholes: Vec<&str> = layout
            .containers
            .iter()
            .filter_map(|c| c.physical_store.as_deref())
            .chain(layout.partitions.iter().map(|p| p.device.as_str()))
            .map(whole_disk_of)
            .collect();
        wholes.sort_unstable();
        wholes.dedup();
        let mut infos = HashMap::new();
        for whole in wholes {
            // A disk that vanished between the calls (unplugged) is left out, not guessed at.
            match diskutil(&["info", "-plist", whole]).await {
                Ok(bytes) => {
                    infos.insert(whole.to_string(), parse_info(&bytes)?);
                }
                Err(DriverError::Backend(e)) => {
                    tracing::debug!("diskutil info {whole} skipped: {e}");
                }
                Err(e) => return Err(e),
            }
        }
        Ok(map_layout(
            &self.backend_id,
            &self.host,
            &layout,
            &infos,
            self.allow_disk_images,
        ))
    }
}

#[async_trait]
impl StorageDriver for RealMacosDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        Ok(self.mapped().await?.discovery)
    }

    async fn health(&self) -> Result<StorageHealth, DriverError> {
        Ok(self.mapped().await?.discovery.health)
    }

    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        Ok(self.mapped().await?.discovery.pools)
    }

    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        Ok(volumes_of(&self.mapped().await?.discovery, pool))
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        Ok(metrics_for(&self.mapped().await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atlas_api_types::VolumeKind;

    #[tokio::test]
    async fn fake_discover_reports_internal_and_external_storage() {
        let d = FakeMacosDriver::new("bkd_macos", "mac01");
        assert!(d.is_fixture());
        let r = d.discover().await.unwrap();
        let names: Vec<_> = r.pools.iter().map(|p| p.name.as_str()).collect();
        for want in ["disk1", "disk2", "disk3", "disk7", "disk8s2"] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        assert!(r.volumes.iter().all(|v| matches!(v.kind, VolumeKind::Filesystem)));
        assert!(r.cluster.raw_capacity_bytes.unwrap() > r.cluster.used_capacity_bytes.unwrap());
    }

    #[tokio::test]
    async fn fake_list_volumes_accepts_pool_name_or_id_and_unknown_is_empty() {
        let d = FakeMacosDriver::with_defaults("bkd_macos");
        let by_name = d.list_volumes("disk7").await.unwrap();
        assert_eq!(by_name.len(), 2);
        let id = d
            .list_pools()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.name == "disk7")
            .unwrap()
            .id;
        assert_eq!(d.list_volumes(&id).await.unwrap().len(), 2);
        assert!(d.list_volumes("disk99").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fake_metrics_cover_counts_and_capacity() {
        let m = FakeMacosDriver::with_defaults("bkd_macos").metrics().await.unwrap();
        let get = |n: &str| m.iter().find(|s| s.name == n).map(|s| s.value);
        assert_eq!(get("macos_containers_total"), Some(4.0));
        assert_eq!(get("macos_smart_failing"), Some(0.0));
        assert!(get("macos_capacity_bytes").unwrap() > 0.0);
    }

    #[tokio::test]
    async fn fake_can_include_disk_images_on_request() {
        let with = FakeMacosDriver::with_defaults("b").allow_disk_images(true);
        let r = with.discover().await.unwrap();
        assert!(r.volumes.iter().any(|v| v.name == "Installer"));
    }

    /// Off a Mac there is no `diskutil`: the real driver reports the backend as unreachable rather
    /// than inventing disks. (On a Mac it would succeed, so the test is skipped there.)
    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn real_without_diskutil_errors_instead_of_fabricating_data() {
        let d = RealMacosDriver::new("bkd_macos", "localhost");
        let err = d.discover().await.unwrap_err();
        assert!(matches!(err, DriverError::Unreachable(_)), "{err:?}");
        assert!(matches!(
            d.list_volumes("disk3").await.unwrap_err(),
            DriverError::Unreachable(_)
        ));
    }
}
