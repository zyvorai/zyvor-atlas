// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Pure parsing and mapping for `diskutil` output. Nothing here runs a command or touches the host, so
//! it is tested against recorded plists on any OS (`tests/fixtures`).
//!
//! Inputs are the property lists of `diskutil list -plist` (whole disks, partitions, mount points),
//! `diskutil apfs list -plist` (containers and volumes with capacity, quota, reserve and roles) and
//! `diskutil info -plist <disk>` (internal/external, SSD, SMART status, bus).

use std::collections::HashMap;

use atlas_api_types::{
    DiscoveryResult, Health, MetricSample, StorageCluster, StorageHealth, StoragePool,
    StorageVolume, VolumeKind,
};
use atlas_driver_core::DriverError;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Raw plist shapes (only the keys we read; everything else is ignored)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawList {
    #[serde(rename = "AllDisksAndPartitions", default)]
    disks: Vec<RawListDisk>,
}

#[derive(Debug, Deserialize)]
struct RawListDisk {
    #[serde(rename = "DeviceIdentifier")]
    id: String,
    #[serde(rename = "Partitions", default)]
    partitions: Vec<RawPartition>,
    #[serde(rename = "APFSVolumes", default)]
    apfs_volumes: Vec<RawListVolume>,
    #[serde(rename = "APFSPhysicalStores", default)]
    stores: Vec<RawStore>,
}

#[derive(Debug, Deserialize)]
struct RawStore {
    #[serde(rename = "DeviceIdentifier")]
    id: String,
}

#[derive(Debug, Deserialize)]
struct RawPartition {
    #[serde(rename = "DeviceIdentifier")]
    id: String,
    #[serde(rename = "Size", default)]
    size: i64,
    #[serde(rename = "Content", default)]
    content: Option<String>,
    #[serde(rename = "VolumeName", default)]
    name: Option<String>,
    #[serde(rename = "MountPoint", default)]
    mount_point: Option<String>,
    #[serde(rename = "DiskUUID", default)]
    uuid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawListVolume {
    #[serde(rename = "DeviceIdentifier")]
    id: String,
    #[serde(rename = "MountPoint", default)]
    mount_point: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawApfs {
    #[serde(rename = "Containers", default)]
    containers: Vec<RawContainer>,
}

#[derive(Debug, Deserialize)]
struct RawContainer {
    #[serde(rename = "APFSContainerUUID", default)]
    uuid: Option<String>,
    #[serde(rename = "ContainerReference")]
    reference: String,
    #[serde(rename = "CapacityCeiling", default)]
    ceiling: i64,
    #[serde(rename = "CapacityFree", default)]
    free: i64,
    #[serde(rename = "DesignatedPhysicalStore", default)]
    designated_store: Option<String>,
    #[serde(rename = "Volumes", default)]
    volumes: Vec<RawApfsVolume>,
}

#[derive(Debug, Deserialize)]
struct RawApfsVolume {
    #[serde(rename = "APFSVolumeUUID", default)]
    uuid: Option<String>,
    #[serde(rename = "DeviceIdentifier")]
    device: String,
    #[serde(rename = "Name", default)]
    name: Option<String>,
    #[serde(rename = "Roles", default)]
    roles: Vec<String>,
    #[serde(rename = "CapacityInUse", default)]
    in_use: i64,
    #[serde(rename = "CapacityQuota", default)]
    quota: i64,
    #[serde(rename = "CapacityReserve", default)]
    reserve: i64,
    #[serde(rename = "Encryption", default)]
    encrypted: bool,
    #[serde(rename = "Locked", default)]
    locked: bool,
}

#[derive(Debug, Deserialize)]
struct RawInfo {
    #[serde(rename = "Internal", default)]
    internal: bool,
    #[serde(rename = "SolidState", default)]
    solid_state: Option<bool>,
    #[serde(rename = "SMARTStatus", default)]
    smart: Option<String>,
    #[serde(rename = "BusProtocol", default)]
    bus: Option<String>,
    #[serde(rename = "MediaName", default)]
    media_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// What `diskutil info` says about one whole disk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiskInfo {
    pub internal: bool,
    pub solid_state: Option<bool>,
    /// `Verified`, `Failing`, `Not Supported`, ... as reported.
    pub smart: Option<String>,
    pub bus: Option<String>,
    pub media_name: Option<String>,
}

impl DiskInfo {
    /// A mounted disk image (`hdiutil attach`), not a physical device.
    pub fn is_disk_image(&self) -> bool {
        self.bus.as_deref() == Some("Disk Image")
    }

    pub fn smart_failing(&self) -> bool {
        self.smart
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("failing"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApfsVolume {
    pub uuid: Option<String>,
    pub device: String,
    pub name: String,
    pub roles: Vec<String>,
    pub in_use: i64,
    /// 0 = no quota.
    pub quota: i64,
    pub reserve: i64,
    pub encrypted: bool,
    pub locked: bool,
    pub mount_point: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApfsContainer {
    pub uuid: Option<String>,
    /// `disk3`.
    pub reference: String,
    pub ceiling: i64,
    pub free: i64,
    /// The partition holding the container, e.g. `disk0s2`.
    pub physical_store: Option<String>,
    pub volumes: Vec<ApfsVolume>,
}

/// A partition with a non-APFS filesystem (HFS+, exFAT, FAT, ...): a data volume of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub device: String,
    pub size: i64,
    pub content: String,
    pub name: Option<String>,
    pub mount_point: Option<String>,
    pub uuid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Layout {
    pub containers: Vec<ApfsContainer>,
    pub partitions: Vec<Partition>,
}

/// `disk0s2` and `disk3s1s1` both belong to whole disk `disk0` / `disk3`.
pub fn whole_disk_of(id: &str) -> &str {
    let rest = id.strip_prefix("disk").unwrap_or(id);
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    &id[..id.len() - rest.len() + digits]
}

fn parse<T: for<'de> Deserialize<'de>>(what: &str, bytes: &[u8]) -> Result<T, DriverError> {
    plist::from_bytes(bytes).map_err(|e| DriverError::Parse(format!("{what}: {e}")))
}

pub fn parse_info(bytes: &[u8]) -> Result<DiskInfo, DriverError> {
    let r: RawInfo = parse("diskutil info", bytes)?;
    Ok(DiskInfo {
        internal: r.internal,
        solid_state: r.solid_state,
        smart: r.smart,
        bus: r.bus,
        media_name: r.media_name,
    })
}

/// Partition contents that are structure, not a filesystem the operator manages.
fn is_filesystem_partition(content: &str) -> bool {
    !(content.starts_with("Apple_APFS")
        || content.starts_with("Apple_Boot")
        || content.starts_with("Apple_Free")
        || content.starts_with("Apple_partition")
        || content.starts_with("Apple_Driver")
        || content.starts_with("Apple_Unknown")
        || content == "EFI"
        || content == "GUID_partition_scheme"
        || content == "FDisk_partition_scheme")
}

pub fn parse_layout(list: &[u8], apfs: &[u8]) -> Result<Layout, DriverError> {
    let list: RawList = parse("diskutil list", list)?;
    let apfs: RawApfs = parse("diskutil apfs list", apfs)?;

    // Mount points only appear in `diskutil list`; join them to APFS volumes by device id.
    let mounts: HashMap<&str, &str> = list
        .disks
        .iter()
        .flat_map(|d| d.apfs_volumes.iter())
        .filter_map(|v| Some((v.id.as_str(), v.mount_point.as_deref()?)))
        .collect();
    let stores: HashMap<&str, &str> = list
        .disks
        .iter()
        .filter_map(|d| Some((d.id.as_str(), d.stores.first()?.id.as_str())))
        .collect();

    let containers = apfs
        .containers
        .into_iter()
        .map(|c| ApfsContainer {
            physical_store: c
                .designated_store
                .or_else(|| stores.get(c.reference.as_str()).map(|s| (*s).to_string())),
            volumes: c
                .volumes
                .into_iter()
                .map(|v| ApfsVolume {
                    mount_point: mounts.get(v.device.as_str()).map(|m| (*m).to_string()),
                    name: v
                        .name
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| v.device.clone()),
                    uuid: v.uuid,
                    device: v.device,
                    roles: v.roles,
                    in_use: v.in_use,
                    quota: v.quota,
                    reserve: v.reserve,
                    encrypted: v.encrypted,
                    locked: v.locked,
                })
                .collect(),
            uuid: c.uuid,
            reference: c.reference,
            ceiling: c.ceiling,
            free: c.free,
        })
        .collect();

    let partitions = list
        .disks
        .iter()
        .flat_map(|d| d.partitions.iter())
        .filter_map(|p| {
            let content = p.content.clone()?;
            is_filesystem_partition(&content).then(|| Partition {
                device: p.id.clone(),
                size: p.size,
                content,
                name: p.name.clone().filter(|n| !n.is_empty()),
                mount_point: p.mount_point.clone(),
                uuid: p.uuid.clone(),
            })
        })
        .collect();

    Ok(Layout {
        containers,
        partitions,
    })
}

// ---------------------------------------------------------------------------
// Mapping to Atlas inventory
// ---------------------------------------------------------------------------

/// Stable id fragment: the UUID when there is one (device numbers change when a drive is replugged),
/// else the device identifier.
fn frag(uuid: Option<&str>, device: &str) -> String {
    uuid.filter(|u| !u.is_empty())
        .unwrap_or(device)
        .to_ascii_lowercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

pub fn cluster_id(host: &str) -> String {
    format!(
        "cls_macos_{}",
        host.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
    )
}

fn device_class(info: Option<&DiskInfo>) -> Option<String> {
    let info = info?;
    match (info.solid_state, info.bus.as_deref()) {
        (Some(true), Some(b)) if b.contains("NVM") || b.contains("Apple Fabric") => {
            Some("nvme".into())
        }
        (Some(true), _) => Some("ssd".into()),
        (Some(false), _) => Some("hdd".into()),
        _ => None,
    }
}

/// A failing SMART status is critical. "Not Supported" (most USB bridges) is not penalised: the
/// disk is simply not assessed, which the `macos_smart_failing` metric does not count either.
fn smart_health(info: Option<&DiskInfo>) -> Health {
    if info.is_some_and(DiskInfo::smart_failing) {
        Health::Critical
    } else {
        Health::Ok
    }
}

/// True unless the whole disk is a mounted disk image and those were not asked for.
fn included(whole: &str, infos: &HashMap<String, DiskInfo>, allow_disk_images: bool) -> bool {
    allow_disk_images || !infos.get(whole).is_some_and(DiskInfo::is_disk_image)
}

/// A SMART or capacity condition the driver can turn into a metric and an alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacAlert {
    pub id: String,
    pub severity: &'static str,
    pub title: String,
    pub detail: String,
    pub resource_id: String,
}

/// Container or quota usage the capacity rule looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityPoint {
    pub id: String,
    pub name: String,
    pub used: i64,
    pub max: i64,
}

pub struct Mapped {
    pub discovery: DiscoveryResult,
    /// Whole disks (included) whose SMART status is failing.
    pub smart_failing: usize,
    /// Whole disks that reported `Verified` (false) or `Failing` (true). `Not Supported` is absent.
    pub assessed_smart: std::collections::BTreeMap<String, bool>,
    /// APFS containers, and volumes that have a quota.
    pub capacity: Vec<CapacityPoint>,
}

const CAPACITY_WARN: f64 = 0.85;
const CAPACITY_CRITICAL: f64 = 0.95;

impl Mapped {
    /// Metric samples (including one `macos_alert` per condition) and the same conditions as alerts.
    pub fn signals(&self) -> (Vec<MetricSample>, Vec<MacAlert>) {
        let mut metrics = Vec::new();
        let mut alerts = Vec::new();
        for (disk, failing) in &self.assessed_smart {
            let mut labels = std::collections::BTreeMap::new();
            labels.insert("disk".into(), disk.clone());
            labels.insert(
                "status".into(),
                if *failing { "failing" } else { "verified" }.into(),
            );
            metrics.push(MetricSample {
                name: "macos_smart_status".into(),
                value: if *failing { 1.0 } else { 0.0 },
                labels,
            });
            if *failing {
                alerts.push(MacAlert {
                    id: format!("macos_smart_{disk}"),
                    severity: "critical",
                    title: format!("SMART failing on {disk}"),
                    detail: format!("diskutil reports SMART status Failing for {disk}"),
                    resource_id: disk.clone(),
                });
            }
        }
        for point in &self.capacity {
            if point.max <= 0 {
                continue;
            }
            let ratio = point.used as f64 / point.max as f64;
            let mut labels = std::collections::BTreeMap::new();
            labels.insert("id".into(), point.id.clone());
            labels.insert("name".into(), point.name.clone());
            metrics.push(MetricSample {
                name: "macos_capacity_used_ratio".into(),
                value: ratio,
                labels,
            });
            if ratio >= CAPACITY_WARN {
                let severity = if ratio >= CAPACITY_CRITICAL {
                    "critical"
                } else {
                    "warning"
                };
                alerts.push(MacAlert {
                    id: format!("macos_capacity_{}", point.id),
                    severity,
                    title: format!("{} is {:.0}% full", point.name, ratio * 100.0),
                    detail: format!(
                        "{} is using {} of {} bytes",
                        point.name, point.used, point.max
                    ),
                    resource_id: point.id.clone(),
                });
            }
        }
        for alert in &alerts {
            let mut labels = std::collections::BTreeMap::new();
            labels.insert("id".into(), alert.id.clone());
            labels.insert("severity".into(), alert.severity.into());
            labels.insert("title".into(), alert.title.clone());
            labels.insert("resource".into(), alert.resource_id.clone());
            metrics.push(MetricSample {
                name: "macos_alert".into(),
                value: if alert.severity == "critical" {
                    2.0
                } else {
                    1.0
                },
                labels,
            });
        }
        (metrics, alerts)
    }
}

/// APFS container -> pool (`apfs_container`), APFS volume -> filesystem volume, other filesystem
/// partition -> its own pool (`partition`) with one volume. Disk images are skipped unless
/// `allow_disk_images` is set (for tests on throwaway images).
pub fn map_layout(
    backend_id: &str,
    host: &str,
    layout: &Layout,
    infos: &HashMap<String, DiskInfo>,
    allow_disk_images: bool,
) -> Mapped {
    let cluster = cluster_id(host);
    let mut pools = Vec::new();
    let mut volumes = Vec::new();
    let mut failing_disks = std::collections::BTreeSet::new();
    let mut assessed_smart = std::collections::BTreeMap::new();
    let mut capacity = Vec::new();

    for c in &layout.containers {
        let whole = c.physical_store.as_deref().map(whole_disk_of);
        if let Some(w) = whole {
            if !included(w, infos, allow_disk_images) {
                continue;
            }
        }
        let info = whole.and_then(|w| infos.get(w));
        let health = smart_health(info);
        if health == Health::Critical {
            failing_disks.extend(whole.map(str::to_string));
        }
        if let Some(w) = whole {
            note_smart(&mut assessed_smart, w, info);
        }
        let pool_id = format!("pool_macos_{}", frag(c.uuid.as_deref(), &c.reference));
        pools.push(StoragePool {
            id: pool_id.clone(),
            cluster_id: cluster.clone(),
            name: c.reference.clone(),
            kind: "apfs_container".into(),
            device_class: device_class(info),
            replica_size: None,
            used_bytes: Some((c.ceiling - c.free).max(0)),
            max_bytes: Some(c.ceiling),
            health,
        });
        if c.ceiling > 0 {
            capacity.push(CapacityPoint {
                id: pool_id.clone(),
                name: c.reference.clone(),
                used: (c.ceiling - c.free).max(0),
                max: c.ceiling,
            });
        }
        for v in &c.volumes {
            volumes.push(StorageVolume {
                id: format!("vol_macos_{}", frag(v.uuid.as_deref(), &v.device)),
                cluster_id: Some(cluster.clone()),
                pool_id: Some(pool_id.clone()),
                name: v.name.clone(),
                kind: VolumeKind::Filesystem,
                backend_native_id: Some(format!("{host}:{}", v.device)),
                // A quota caps the volume; otherwise it can grow to the whole container.
                size_bytes: if v.quota > 0 { v.quota } else { c.ceiling },
                used_bytes: Some(v.in_use),
                state: if v.locked {
                    "locked".into()
                } else if v.mount_point.is_some() {
                    "available".into()
                } else {
                    "unmounted".into()
                },
                health: Health::Ok,
                kubernetes_namespace: None,
                pvc_name: None,
                storage_class_name: v.mount_point.clone(),
            });
            if v.quota > 0 {
                capacity.push(CapacityPoint {
                    id: format!("vol_macos_{}", frag(v.uuid.as_deref(), &v.device)),
                    name: v.name.clone(),
                    used: v.in_use.max(0),
                    max: v.quota,
                });
            }
        }
    }

    for p in &layout.partitions {
        let whole = whole_disk_of(&p.device);
        if !included(whole, infos, allow_disk_images) {
            continue;
        }
        let info = infos.get(whole);
        let health = smart_health(info);
        if health == Health::Critical {
            failing_disks.insert(whole.to_string());
        }
        note_smart(&mut assessed_smart, whole, info);
        let id = frag(p.uuid.as_deref(), &p.device);
        let pool_id = format!("pool_macos_{id}");
        pools.push(StoragePool {
            id: pool_id.clone(),
            cluster_id: cluster.clone(),
            name: p.device.clone(),
            kind: "partition".into(),
            device_class: device_class(info),
            replica_size: None,
            used_bytes: None,
            max_bytes: Some(p.size),
            health,
        });
        volumes.push(StorageVolume {
            id: format!("vol_macos_{id}"),
            cluster_id: Some(cluster.clone()),
            pool_id: Some(pool_id),
            name: p.name.clone().unwrap_or_else(|| p.device.clone()),
            kind: VolumeKind::Filesystem,
            backend_native_id: Some(format!("{host}:{}", p.device)),
            size_bytes: p.size,
            used_bytes: None,
            state: if p.mount_point.is_some() {
                "available".into()
            } else {
                "unmounted".into()
            },
            health: Health::Ok,
            kubernetes_namespace: None,
            pvc_name: None,
            storage_class_name: p.mount_point.clone(),
        });
    }

    let raw: i64 = pools.iter().filter_map(|p| p.max_bytes).sum();
    let used: i64 = pools.iter().filter_map(|p| p.used_bytes).sum();
    let any = !pools.is_empty();
    let smart_failing = failing_disks.len();
    let status = if smart_failing > 0 {
        Health::Critical
    } else {
        Health::Ok
    };
    let containers = pools.iter().filter(|p| p.kind == "apfs_container").count();
    let summary = format!(
        "{containers} APFS container(s), {} volume(s) on {host}{}",
        volumes.len(),
        if smart_failing > 0 {
            format!("; SMART failing on {smart_failing} disk(s)")
        } else {
            String::new()
        }
    );
    let health = StorageHealth {
        status,
        summary,
        raw_capacity_bytes: any.then_some(raw),
        used_capacity_bytes: any.then_some(used),
        available_capacity_bytes: any.then_some(raw - used),
        recovering: false,
        degraded_objects: 0,
    };
    Mapped {
        discovery: DiscoveryResult {
            cluster: StorageCluster {
                id: cluster,
                backend_id: backend_id.into(),
                name: format!("macos://{host}"),
                native_fsid: None,
                health: status,
                raw_capacity_bytes: any.then_some(raw),
                used_capacity_bytes: any.then_some(used),
                available_capacity_bytes: any.then_some(raw - used),
            },
            pools,
            osds: vec![],
            volumes,
            health,
        },
        smart_failing,
        assessed_smart,
        capacity,
    }
}

fn note_smart(
    out: &mut std::collections::BTreeMap<String, bool>,
    whole: &str,
    info: Option<&DiskInfo>,
) {
    let Some(status) = info.and_then(|i| i.smart.as_deref()) else {
        return;
    };
    if status.eq_ignore_ascii_case("failing") {
        out.insert(whole.to_string(), true);
    } else if status.eq_ignore_ascii_case("verified") {
        out.entry(whole.to_string()).or_insert(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &[u8] = include_bytes!("../tests/fixtures/list.plist");
    const APFS: &[u8] = include_bytes!("../tests/fixtures/apfs.plist");

    pub(crate) fn infos() -> HashMap<String, DiskInfo> {
        let mut m = HashMap::new();
        for (id, b) in [
            (
                "disk0",
                &include_bytes!("../tests/fixtures/info_disk0.plist")[..],
            ),
            (
                "disk6",
                &include_bytes!("../tests/fixtures/info_disk6.plist")[..],
            ),
            (
                "disk8",
                &include_bytes!("../tests/fixtures/info_disk8.plist")[..],
            ),
            (
                "disk9",
                &include_bytes!("../tests/fixtures/info_disk9.plist")[..],
            ),
        ] {
            m.insert(id.to_string(), parse_info(b).unwrap());
        }
        m
    }

    #[test]
    fn whole_disk_of_strips_slices_and_snapshots() {
        assert_eq!(whole_disk_of("disk0"), "disk0");
        assert_eq!(whole_disk_of("disk0s2"), "disk0");
        assert_eq!(whole_disk_of("disk3s1s1"), "disk3");
        assert_eq!(whole_disk_of("disk12s3"), "disk12");
    }

    #[test]
    fn parses_containers_volumes_quota_and_mounts() {
        let l = parse_layout(LIST, APFS).unwrap();
        let refs: Vec<_> = l.containers.iter().map(|c| c.reference.as_str()).collect();
        assert_eq!(refs, ["disk1", "disk2", "disk3", "disk7"]);
        let ext = l
            .containers
            .iter()
            .find(|c| c.reference == "disk7")
            .unwrap();
        assert_eq!(ext.physical_store.as_deref(), Some("disk6s2"));
        assert_eq!(ext.volumes.len(), 2);
        let scratch = ext.volumes.iter().find(|v| v.name == "Scratch").unwrap();
        assert_eq!(
            (scratch.quota, scratch.reserve),
            (200_000_000_000, 50_000_000_000)
        );
        assert_eq!(scratch.mount_point.as_deref(), Some("/Volumes/Scratch"));
        assert!(
            ext.volumes
                .iter()
                .find(|v| v.name == "Backup")
                .unwrap()
                .encrypted
        );
        // the internal container carries the system roles and its physical store
        let internal = l
            .containers
            .iter()
            .find(|c| c.reference == "disk3")
            .unwrap();
        assert_eq!(internal.physical_store.as_deref(), Some("disk0s2"));
        assert!(internal
            .volumes
            .iter()
            .any(|v| v.roles.iter().any(|r| r == "System")));
    }

    #[test]
    fn only_real_filesystem_partitions_become_partitions() {
        let l = parse_layout(LIST, APFS).unwrap();
        let devs: Vec<_> = l.partitions.iter().map(|p| p.device.as_str()).collect();
        // EFI and the APFS/ISC/Recovery partitions are structure; exFAT and HFS+ are filesystems.
        assert_eq!(devs, ["disk8s2", "disk9s1"]);
    }

    #[test]
    fn maps_to_pools_and_volumes_and_skips_disk_images() {
        let l = parse_layout(LIST, APFS).unwrap();
        let m = map_layout("bkd_macos", "mac01", &l, &infos(), false);
        let d = &m.discovery;
        let ext = d.pools.iter().find(|p| p.name == "disk7").unwrap();
        assert_eq!(ext.kind, "apfs_container");
        assert_eq!(ext.max_bytes, Some(999_995_129_856));
        assert_eq!(ext.used_bytes, Some(413_200_000_000));
        assert_eq!(ext.device_class.as_deref(), Some("ssd"));
        let scratch = d.volumes.iter().find(|v| v.name == "Scratch").unwrap();
        assert_eq!(scratch.size_bytes, 200_000_000_000); // quota caps it
        let backup = d.volumes.iter().find(|v| v.name == "Backup").unwrap();
        assert_eq!(backup.size_bytes, 999_995_129_856); // no quota: whole container
        assert_eq!(backup.backend_native_id.as_deref(), Some("mac01:disk7s1"));
        let cam = d.pools.iter().find(|p| p.name == "disk8s2").unwrap();
        assert_eq!(
            (cam.kind.as_str(), cam.device_class.as_deref()),
            ("partition", Some("hdd"))
        );
        assert!(d.volumes.iter().any(|v| v.name == "CAMERA"));
        // the mounted disk image is not a device Atlas manages
        assert!(d.volumes.iter().all(|v| v.name != "Installer"));
        assert_eq!(d.cluster.name, "macos://mac01");
        assert_eq!(m.smart_failing, 0);
        // every volume points at a pool in this result
        for v in &d.volumes {
            assert!(
                d.pools.iter().any(|p| Some(&p.id) == v.pool_id.as_ref()),
                "{}",
                v.name
            );
        }
    }

    #[test]
    fn disk_images_appear_only_when_allowed() {
        let l = parse_layout(LIST, APFS).unwrap();
        let m = map_layout("bkd_macos", "mac01", &l, &infos(), true);
        assert!(m.discovery.volumes.iter().any(|v| v.name == "Installer"));
    }

    #[test]
    fn failing_smart_makes_the_pool_and_the_cluster_critical() {
        let l = parse_layout(LIST, APFS).unwrap();
        let mut i = infos();
        i.get_mut("disk6").unwrap().smart = Some("Failing".into());
        let m = map_layout("bkd_macos", "mac01", &l, &i, false);
        let ext = m
            .discovery
            .pools
            .iter()
            .find(|p| p.name == "disk7")
            .unwrap();
        assert_eq!(ext.health, Health::Critical);
        let internal = m
            .discovery
            .pools
            .iter()
            .find(|p| p.name == "disk3")
            .unwrap();
        assert_eq!(internal.health, Health::Ok);
        assert_eq!(m.discovery.health.status, Health::Critical);
        assert_eq!(m.smart_failing, 1);
    }

    #[test]
    fn unsupported_smart_is_not_penalised() {
        let i = infos();
        assert_eq!(i["disk6"].smart.as_deref(), Some("Not Supported"));
        assert!(!i["disk6"].smart_failing());
        assert_eq!(i["disk0"].smart.as_deref(), Some("Verified"));
        assert!(i["disk0"].internal && !i["disk6"].internal);
        assert!(i["disk9"].is_disk_image());
    }

    #[test]
    fn smart_and_capacity_signals_become_metrics_and_alerts() {
        let layout = Layout {
            containers: vec![ApfsContainer {
                uuid: Some("u1".into()),
                reference: "disk3".into(),
                ceiling: 1000,
                free: 800,
                physical_store: Some("disk0s2".into()),
                volumes: vec![ApfsVolume {
                    uuid: None,
                    device: "disk3s1".into(),
                    name: "Data".into(),
                    roles: vec![],
                    in_use: 90,
                    quota: 100,
                    reserve: 0,
                    encrypted: false,
                    locked: false,
                    mount_point: Some("/Volumes/Data".into()),
                }],
            }],
            partitions: vec![],
        };
        let mut infos = HashMap::new();
        infos.insert(
            "disk0".into(),
            DiskInfo {
                internal: true,
                solid_state: Some(true),
                smart: Some("Failing".into()),
                bus: Some("Apple Fabric".into()),
                media_name: None,
            },
        );
        let m = map_layout("bkd", "mac", &layout, &infos, false);
        let (metrics, alerts) = m.signals();
        assert!(alerts
            .iter()
            .any(|a| a.id == "macos_smart_disk0" && a.severity == "critical"));
        assert!(alerts
            .iter()
            .any(|a| a.severity == "warning" && a.resource_id.contains("disk3s1")));
        assert!(metrics
            .iter()
            .any(|s| s.name == "macos_smart_status" && s.value == 1.0));
        assert!(metrics
            .iter()
            .any(|s| s.name == "macos_capacity_used_ratio" && s.value >= 0.85));
        assert!(metrics
            .iter()
            .any(|s| s.name == "macos_alert" && s.value == 2.0));
        // A verified disk with room to spare does not alert.
        infos.get_mut("disk0").unwrap().smart = Some("Verified".into());
        let layout = Layout {
            containers: vec![ApfsContainer {
                uuid: Some("u1".into()),
                reference: "disk3".into(),
                ceiling: 1000,
                free: 800,
                physical_store: Some("disk0s2".into()),
                volumes: vec![],
            }],
            partitions: vec![],
        };
        let calm = map_layout("bkd", "mac", &layout, &infos, false);
        let (_, calm_alerts) = calm.signals();
        assert!(calm_alerts.is_empty());
        assert!(calm
            .signals()
            .0
            .iter()
            .any(|s| s.name == "macos_smart_status" && s.value == 0.0));
    }

    #[test]
    fn garbage_is_a_parse_error_not_an_empty_inventory() {
        assert!(matches!(
            parse_layout(b"not a plist", APFS),
            Err(DriverError::Parse(_))
        ));
        assert!(matches!(parse_info(b""), Err(DriverError::Parse(_))));
    }
}
