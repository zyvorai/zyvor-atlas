// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Volume lifecycle on a Mac's **external** disks: erase a disk, add/delete an APFS volume, eject.
//!
//! Every operation first inspects the live layout and runs a pure refusal function over it. The
//! refusals are hard: nothing in the request can override them, and the gateway job runs them again
//! right before executing. Commands are argument arrays and never use `-force`. In fake mode the
//! same checks run against the recorded fixtures and nothing is executed.

use std::collections::HashMap;

use atlas_driver_core::DriverError;

use crate::parse::{whole_disk_of, ApfsContainer, ApfsVolume, DiskInfo, Layout};
use crate::{diskutil, parse_info, parse_layout};

/// APFS roles that belong to the OS; such volumes are never deleted here.
const SYSTEM_ROLES: &[&str] = &[
    "System", "Data", "Preboot", "Recovery", "VM", "Update", "xART", "Hardware", "ISC", "Backup",
];

/// Mount points that mean "the running system uses this".
fn is_system_mount(m: &str) -> bool {
    m == "/" || m.starts_with("/System/") || m.starts_with("/private/") || m.starts_with("/usr")
}

fn valid_disk_id(id: &str) -> bool {
    id.strip_prefix("disk")
        .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
}

fn valid_slice_id(id: &str) -> bool {
    let w = whole_disk_of(id);
    valid_disk_id(w)
        && id[w.len()..].strip_prefix('s').is_some_and(|r| {
            let r = r.split('s').collect::<Vec<_>>();
            r.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        })
}

/// A volume name `diskutil` receives as one argument. Starts alphanumeric so it can never be an option.
pub fn validate_volume_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '.' | '-'));
    if ok {
        Ok(())
    } else {
        Err("volume name must be 1-32 characters of letters, digits, space, '_', '.' or '-', starting with a letter or digit".into())
    }
}

/// Whole disks that hold any container/partition and are mounted for the running system.
fn boot_disks(layout: &Layout) -> Vec<String> {
    let mut out = Vec::new();
    for c in &layout.containers {
        let boot = c
            .volumes
            .iter()
            .any(|v| v.mount_point.as_deref().is_some_and(is_system_mount));
        if boot {
            if let Some(s) = &c.physical_store {
                out.push(whole_disk_of(s).to_string());
            }
        }
    }
    for p in &layout.partitions {
        if p.mount_point.as_deref().is_some_and(is_system_mount) {
            out.push(whole_disk_of(&p.device).to_string());
        }
    }
    out
}

/// Refusals that depend on the whole disk only.
fn refuse_disk(
    whole: &str,
    info: &DiskInfo,
    layout: &Layout,
    allow_disk_images: bool,
) -> Result<(), String> {
    if info.internal {
        return Err(format!("{whole} is an internal disk"));
    }
    if info.is_disk_image() && !allow_disk_images {
        return Err(format!("{whole} is a disk image"));
    }
    if boot_disks(layout).iter().any(|b| b == whole) {
        return Err(format!("{whole} holds the running system"));
    }
    Ok(())
}

/// Mount points on `whole` that are not an ordinary `/Volumes/...` mount.
fn busy_mount(whole: &str, layout: &Layout) -> Option<String> {
    let parts = layout.partitions.iter().filter(|p| whole_disk_of(&p.device) == whole);
    let vols = layout
        .containers
        .iter()
        .filter(|c| c.physical_store.as_deref().map(whole_disk_of) == Some(whole))
        .flat_map(|c| c.volumes.iter().filter_map(|v| v.mount_point.clone()));
    parts
        .filter_map(|p| p.mount_point.clone())
        .chain(vols)
        .find(|m| !m.starts_with("/Volumes/"))
}

/// May `whole` be erased?
pub fn refuse_erase(
    whole: &str,
    info: &DiskInfo,
    layout: &Layout,
    allow_disk_images: bool,
) -> Result<(), String> {
    if !valid_disk_id(whole) {
        return Err(format!("{whole:?} is not a whole-disk identifier like disk4"));
    }
    refuse_disk(whole, info, layout, allow_disk_images)?;
    if let Some(m) = busy_mount(whole, layout) {
        return Err(format!("{whole} has a volume mounted at {m}"));
    }
    Ok(())
}

/// May `whole` be ejected? Same disk rules, no mount rule: eject unmounts.
pub fn refuse_eject(
    whole: &str,
    info: &DiskInfo,
    layout: &Layout,
    allow_disk_images: bool,
) -> Result<(), String> {
    if !valid_disk_id(whole) {
        return Err(format!("{whole:?} is not a whole-disk identifier like disk4"));
    }
    refuse_disk(whole, info, layout, allow_disk_images)
}

fn container_of<'a>(layout: &'a Layout, reference: &str) -> Option<&'a ApfsContainer> {
    layout.containers.iter().find(|c| c.reference == reference)
}

fn container_whole(c: &ApfsContainer) -> Result<&str, String> {
    c.physical_store
        .as_deref()
        .map(whole_disk_of)
        .ok_or_else(|| format!("container {} has no physical store", c.reference))
}

/// May a volume be added to APFS container `container`?
pub fn refuse_add_volume(
    container: &str,
    infos: &HashMap<String, DiskInfo>,
    layout: &Layout,
    allow_disk_images: bool,
) -> Result<(), String> {
    if !valid_disk_id(container) {
        return Err(format!("{container:?} is not a container identifier like disk5"));
    }
    let c = container_of(layout, container)
        .ok_or_else(|| format!("{container} is not an APFS container"))?;
    let whole = container_whole(c)?;
    let info = infos.get(whole).ok_or_else(|| format!("no information for {whole}"))?;
    refuse_disk(whole, info, layout, allow_disk_images)
}

/// May APFS volume `volume` (`disk5s2`) be deleted? Returns the volume.
pub fn refuse_delete_volume<'a>(
    volume: &str,
    infos: &HashMap<String, DiskInfo>,
    layout: &'a Layout,
    allow_disk_images: bool,
) -> Result<&'a ApfsVolume, String> {
    if !valid_slice_id(volume) {
        return Err(format!("{volume:?} is not a volume identifier like disk5s2"));
    }
    let (c, v) = layout
        .containers
        .iter()
        .find_map(|c| c.volumes.iter().find(|v| v.device == volume).map(|v| (c, v)))
        .ok_or_else(|| format!("{volume} is not an APFS volume"))?;
    let whole = container_whole(c)?;
    let info = infos.get(whole).ok_or_else(|| format!("no information for {whole}"))?;
    refuse_disk(whole, info, layout, allow_disk_images)?;
    if let Some(r) = v.roles.iter().find(|r| SYSTEM_ROLES.contains(&r.as_str())) {
        return Err(format!("{volume} is a {r} system volume"));
    }
    if let Some(m) = v.mount_point.as_deref().filter(|m| !m.starts_with("/Volumes/")) {
        return Err(format!("{volume} is mounted at {m}"));
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Commands (pure)
// ---------------------------------------------------------------------------

pub fn erase_args(whole: &str, name: &str) -> Vec<String> {
    ["eraseDisk", "APFS", name, "GPT", whole].map(String::from).to_vec()
}

pub fn add_volume_args(
    container: &str,
    name: &str,
    quota: Option<u64>,
    reserve: Option<u64>,
) -> Vec<String> {
    let mut a: Vec<String> = ["apfs", "addVolume", container, "APFS", name]
        .map(String::from)
        .to_vec();
    if let Some(q) = quota {
        a.extend(["-quota".into(), format!("{q}B")]);
    }
    if let Some(r) = reserve {
        a.extend(["-reserve".into(), format!("{r}B")]);
    }
    a
}

pub fn delete_volume_args(volume: &str) -> Vec<String> {
    ["apfs", "deleteVolume", volume].map(String::from).to_vec()
}

pub fn eject_args(whole: &str) -> Vec<String> {
    ["eject", whole].map(String::from).to_vec()
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// Runs the lifecycle operations against `diskutil` (`real`) or the recorded fixtures (`fake`).
#[derive(Debug, Clone)]
pub struct MacosOps {
    fake: bool,
    allow_disk_images: bool,
}

impl MacosOps {
    pub fn fake(allow_disk_images: bool) -> Self {
        Self { fake: true, allow_disk_images }
    }

    pub fn real(allow_disk_images: bool) -> Self {
        Self { fake: false, allow_disk_images }
    }

    async fn snapshot(&self) -> Result<(Layout, HashMap<String, DiskInfo>), DriverError> {
        if self.fake {
            return crate::fixture_state();
        }
        let list = diskutil(&["list", "-plist"]).await?;
        let apfs = diskutil(&["apfs", "list", "-plist"]).await?;
        let layout = parse_layout(&list, &apfs)?;
        let mut infos = HashMap::new();
        let mut wholes: Vec<&str> = layout
            .containers
            .iter()
            .filter_map(|c| c.physical_store.as_deref())
            .chain(layout.partitions.iter().map(|p| p.device.as_str()))
            .map(whole_disk_of)
            .collect();
        wholes.sort_unstable();
        wholes.dedup();
        for w in wholes {
            if let Ok(b) = diskutil(&["info", "-plist", w]).await {
                infos.insert(w.to_string(), parse_info(&b)?);
            }
        }
        Ok((layout, infos))
    }

    async fn run(&self, args: Vec<String>) -> Result<String, DriverError> {
        if self.fake {
            return Ok(format!("fake: diskutil {}", args.join(" ")));
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = diskutil(&refs).await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    fn refused(msg: String) -> DriverError {
        DriverError::Backend(format!("refused: {msg}"))
    }

    /// Refusal checks only, nothing executed. The gateway route calls these to fail fast; the job
    /// calls them again through the operation itself.
    pub async fn check_erase(&self, whole: &str, name: &str) -> Result<(), DriverError> {
        validate_volume_name(name).map_err(DriverError::Backend)?;
        let (layout, infos) = self.snapshot().await?;
        let info = infos
            .get(whole)
            .ok_or_else(|| Self::refused(format!("{whole} is not a disk Atlas can see")))?;
        refuse_erase(whole, info, &layout, self.allow_disk_images).map_err(Self::refused)
    }

    pub async fn check_add_volume(&self, container: &str, name: &str) -> Result<(), DriverError> {
        validate_volume_name(name).map_err(DriverError::Backend)?;
        let (layout, infos) = self.snapshot().await?;
        refuse_add_volume(container, &infos, &layout, self.allow_disk_images)
            .map_err(Self::refused)
    }

    pub async fn check_delete_volume(&self, volume: &str) -> Result<(), DriverError> {
        let (layout, infos) = self.snapshot().await?;
        refuse_delete_volume(volume, &infos, &layout, self.allow_disk_images)
            .map(|_| ())
            .map_err(Self::refused)
    }

    pub async fn check_eject(&self, whole: &str) -> Result<(), DriverError> {
        let (layout, infos) = self.snapshot().await?;
        let info = infos
            .get(whole)
            .ok_or_else(|| Self::refused(format!("{whole} is not a disk Atlas can see")))?;
        refuse_eject(whole, info, &layout, self.allow_disk_images).map_err(Self::refused)
    }

    pub async fn erase_disk(&self, whole: &str, name: &str) -> Result<String, DriverError> {
        self.check_erase(whole, name).await?;
        self.run(erase_args(whole, name)).await
    }

    pub async fn add_volume(
        &self,
        container: &str,
        name: &str,
        quota: Option<u64>,
        reserve: Option<u64>,
    ) -> Result<String, DriverError> {
        self.check_add_volume(container, name).await?;
        self.run(add_volume_args(container, name, quota, reserve)).await
    }

    pub async fn delete_volume(&self, volume: &str) -> Result<String, DriverError> {
        self.check_delete_volume(volume).await?;
        self.run(delete_volume_args(volume)).await
    }

    pub async fn eject(&self, whole: &str) -> Result<String, DriverError> {
        self.check_eject(whole).await?;
        self.run(eject_args(whole)).await
    }

    /// Every whole disk with what may be done to it, for the picker.
    pub async fn devices(&self) -> Result<Vec<DeviceRow>, DriverError> {
        let (layout, infos) = self.snapshot().await?;
        let mut rows: Vec<DeviceRow> = infos
            .iter()
            .filter(|(_, i)| self.allow_disk_images || !i.is_disk_image())
            .map(|(whole, info)| DeviceRow {
                device: whole.clone(),
                internal: info.internal,
                bus: info.bus.clone(),
                media_name: info.media_name.clone(),
                smart: info.smart.clone(),
                disk_image: info.is_disk_image(),
                containers: layout
                    .containers
                    .iter()
                    .filter(|c| c.physical_store.as_deref().map(whole_disk_of) == Some(whole))
                    .map(|c| c.reference.clone())
                    .collect(),
                erase_refusal: refuse_erase(whole, info, &layout, self.allow_disk_images).err(),
            })
            .collect();
        rows.sort_by(|a, b| a.device.cmp(&b.device));
        Ok(rows)
    }
}

/// The lifecycle operations, local (`MacosOps`) or on another Mac through its agent
/// (`RemoteMacosOps`). `check_*` run the refusals only; the operations run them again first.
#[async_trait::async_trait]
pub trait Lifecycle: Send + Sync {
    async fn devices(&self) -> Result<Vec<DeviceRow>, DriverError>;
    async fn check_erase(&self, whole: &str, name: &str) -> Result<(), DriverError>;
    async fn check_add_volume(&self, container: &str, name: &str) -> Result<(), DriverError>;
    async fn check_delete_volume(&self, volume: &str) -> Result<(), DriverError>;
    async fn check_eject(&self, whole: &str) -> Result<(), DriverError>;
    async fn erase_disk(&self, whole: &str, name: &str) -> Result<String, DriverError>;
    async fn add_volume(
        &self,
        container: &str,
        name: &str,
        quota: Option<u64>,
        reserve: Option<u64>,
    ) -> Result<String, DriverError>;
    async fn delete_volume(&self, volume: &str) -> Result<String, DriverError>;
    async fn eject(&self, whole: &str) -> Result<String, DriverError>;
}

#[async_trait::async_trait]
impl Lifecycle for MacosOps {
    async fn devices(&self) -> Result<Vec<DeviceRow>, DriverError> {
        MacosOps::devices(self).await
    }
    async fn check_erase(&self, whole: &str, name: &str) -> Result<(), DriverError> {
        MacosOps::check_erase(self, whole, name).await
    }
    async fn check_add_volume(&self, container: &str, name: &str) -> Result<(), DriverError> {
        MacosOps::check_add_volume(self, container, name).await
    }
    async fn check_delete_volume(&self, volume: &str) -> Result<(), DriverError> {
        MacosOps::check_delete_volume(self, volume).await
    }
    async fn check_eject(&self, whole: &str) -> Result<(), DriverError> {
        MacosOps::check_eject(self, whole).await
    }
    async fn erase_disk(&self, whole: &str, name: &str) -> Result<String, DriverError> {
        MacosOps::erase_disk(self, whole, name).await
    }
    async fn add_volume(
        &self,
        container: &str,
        name: &str,
        quota: Option<u64>,
        reserve: Option<u64>,
    ) -> Result<String, DriverError> {
        MacosOps::add_volume(self, container, name, quota, reserve).await
    }
    async fn delete_volume(&self, volume: &str) -> Result<String, DriverError> {
        MacosOps::delete_volume(self, volume).await
    }
    async fn eject(&self, whole: &str) -> Result<String, DriverError> {
        MacosOps::eject(self, whole).await
    }
}

/// One whole disk as the picker shows it. `erase_refusal` is why Atlas will not erase it, if so.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeviceRow {
    pub device: String,
    pub internal: bool,
    pub bus: Option<String>,
    pub media_name: Option<String>,
    pub smart: Option<String>,
    pub disk_image: bool,
    pub containers: Vec<String>,
    pub erase_refusal: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (Layout, HashMap<String, DiskInfo>) {
        crate::fixture_state().unwrap()
    }

    #[test]
    fn internal_disk_and_boot_container_are_always_refused() {
        let (l, i) = state();
        assert!(refuse_erase("disk0", &i["disk0"], &l, true).unwrap_err().contains("internal"));
        assert!(refuse_eject("disk0", &i["disk0"], &l, true).is_err());
        assert!(refuse_add_volume("disk3", &i, &l, true).is_err());
        for v in ["disk3s1", "disk3s5", "disk3s4"] {
            assert!(refuse_delete_volume(v, &i, &l, true).is_err(), "{v}");
        }
    }

    #[test]
    fn external_disk_can_be_erased_ejected_and_extended() {
        let (l, i) = state();
        assert!(refuse_erase("disk6", &i["disk6"], &l, false).is_ok());
        assert!(refuse_erase("disk8", &i["disk8"], &l, false).is_ok());
        assert!(refuse_eject("disk6", &i["disk6"], &l, false).is_ok());
        assert!(refuse_add_volume("disk7", &i, &l, false).is_ok());
        assert_eq!(refuse_delete_volume("disk7s2", &i, &l, false).unwrap().name, "Scratch");
    }

    #[test]
    fn disk_images_need_the_test_flag() {
        let (l, i) = state();
        assert!(refuse_erase("disk9", &i["disk9"], &l, false).unwrap_err().contains("disk image"));
        assert!(refuse_erase("disk9", &i["disk9"], &l, true).is_ok());
    }

    #[test]
    fn system_roles_and_odd_mounts_refuse_volume_delete() {
        let (mut l, i) = state();
        let c = l.containers.iter_mut().find(|c| c.reference == "disk7").unwrap();
        c.volumes[0].roles = vec!["Preboot".into()];
        assert!(refuse_delete_volume("disk7s1", &i, &l, false).unwrap_err().contains("system volume"));
        let c = l.containers.iter_mut().find(|c| c.reference == "disk7").unwrap();
        c.volumes[0].roles.clear();
        c.volumes[0].mount_point = Some("/Applications/X".into());
        assert!(refuse_delete_volume("disk7s1", &i, &l, false).unwrap_err().contains("mounted at"));
    }

    #[test]
    fn a_disk_with_a_system_mount_is_not_erasable() {
        let (mut l, i) = state();
        let c = l.containers.iter_mut().find(|c| c.reference == "disk7").unwrap();
        c.volumes[0].mount_point = Some("/".into());
        assert!(refuse_erase("disk6", &i["disk6"], &l, false).unwrap_err().contains("running system"));
        let (mut l, i) = state();
        let c = l.containers.iter_mut().find(|c| c.reference == "disk7").unwrap();
        c.volumes[0].mount_point = Some("/Applications/X".into());
        assert!(refuse_erase("disk6", &i["disk6"], &l, false).unwrap_err().contains("mounted at"));
    }

    #[test]
    fn identifiers_and_names_are_validated() {
        let (l, i) = state();
        for bad in ["", "disk", "disk6s2", "disk6; rm", "../disk6", "-disk6"] {
            assert!(refuse_erase(bad, &i["disk6"], &l, false).is_err(), "{bad}");
        }
        assert!(refuse_delete_volume("disk7", &i, &l, false).is_err());
        assert!(refuse_delete_volume("disk7s9", &i, &l, false).is_err());
        for bad in ["", "-x", "a/b", "a;b", " lead", &"x".repeat(33)] {
            assert!(validate_volume_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_volume_name("Backup 2.1_x-y").is_ok());
    }

    #[test]
    fn commands_are_argument_arrays() {
        assert_eq!(erase_args("disk4", "Data"), ["eraseDisk", "APFS", "Data", "GPT", "disk4"]);
        assert_eq!(
            add_volume_args("disk5", "V", Some(10), Some(5)),
            ["apfs", "addVolume", "disk5", "APFS", "V", "-quota", "10B", "-reserve", "5B"]
        );
        assert_eq!(delete_volume_args("disk5s2"), ["apfs", "deleteVolume", "disk5s2"]);
        assert_eq!(eject_args("disk4"), ["eject", "disk4"]);
    }

    #[tokio::test]
    async fn fake_ops_check_fixtures_and_execute_nothing() {
        let ops = MacosOps::fake(false);
        assert!(ops.erase_disk("disk6", "Fresh").await.unwrap().starts_with("fake:"));
        assert!(ops.erase_disk("disk0", "Nope").await.is_err());
        assert!(ops.delete_volume("disk3s5").await.is_err());
        assert!(ops.delete_volume("disk7s2").await.is_ok());
        assert!(ops.add_volume("disk7", "New", Some(1 << 30), None).await.is_ok());
        assert!(ops.erase_disk("disk42", "X").await.is_err());
    }
}
