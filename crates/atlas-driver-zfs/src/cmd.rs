// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Safe command wrappers for provisioning a raw, unformatted local block device into a new zpool.
//! Local-host-only, same as the rest of this driver (see the module doc in `lib.rs`).
//!
//! Security rule (PDF §17.3, same as `atlas-driver-ceph`'s `cmd.rs`): every command is an
//! argument array, never a shell string.

use atlas_driver_core::DriverError;

use crate::sanitize;

fn cmd(bin: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(bin);
    c.kill_on_drop(true);
    c
}

async fn run(bin: &str, args: &[&str]) -> Result<(bool, String, String), DriverError> {
    let output = cmd(bin)
        .args(args)
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `{bin}`: {e}")))?;
    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

/// Everything observed about a candidate device before any mutating command runs. Data-gathering
/// is kept separate from the pass/fail decision (`refusal_reason`) so the decision logic itself is
/// unit-testable without shelling out to real commands.
#[derive(Debug, Clone, Default)]
pub struct DeviceCheck {
    pub exists: bool,
    pub is_disk: bool,
    pub read_only: bool,
    pub has_children: bool,
    pub fstype: Option<String>,
    pub pttype: Option<String>,
    pub mounted_at: Option<String>,
    pub wipefs_signatures: Vec<String>,
    pub member_of_zpool: Option<String>,
    pub is_root_or_boot_disk: bool,
}

impl DeviceCheck {
    /// The refusals that `wipe_existing` can never override, checked first regardless of what
    /// else is true about the device: touching the root/boot disk, a read-only device, something
    /// that isn't a whole disk at all, a device that's currently mounted (it may be actively
    /// serving files right now — unmounting it is a separate, deliberately out-of-scope action),
    /// or a member of a zpool that is *currently imported* (`member_of_zpool` comes from
    /// `zpool status`, which only lists imported pools — wiping a live pool's disk corrupts a
    /// running pool; a stale label from an exported/destroyed pool shows up as a `zfs_member`
    /// filesystem signature instead, which is the overridable, wipeable case).
    pub fn hard_refusal_reason(&self) -> Option<String> {
        if !self.exists {
            return Some("no such block device".into());
        }
        if self.is_root_or_boot_disk {
            return Some("device backs the host's root or boot filesystem — refusing".into());
        }
        if self.read_only {
            return Some("device is read-only".into());
        }
        if !self.is_disk {
            return Some(
                "device is not a whole disk (a partition, loop, or optical device) — refusing"
                    .into(),
            );
        }
        if let Some(target) = &self.mounted_at {
            return Some(format!("device is mounted at {target}"));
        }
        if let Some(zpool) = &self.member_of_zpool {
            return Some(format!(
                "device is a member of the imported zpool {zpool} — export or destroy that pool \
                 first; wiping a live pool's disk would corrupt it"
            ));
        }
        None
    }

    /// `None` means the device looks safe to format; `Some(reason)` explains why it doesn't.
    /// Checked in order so the *first* applicable reason is the one surfaced. Everything past
    /// `hard_refusal_reason` here is stale *data* on the disk (a partition table, a filesystem or
    /// RAID/LVM signature) rather than a structural reason the device is
    /// unsafe to touch at all — `wipe_existing` (see `wipe_device`) clears these, so callers taking
    /// that path check `hard_refusal_reason` alone instead of this.
    pub fn refusal_reason(&self) -> Option<String> {
        if let Some(reason) = self.hard_refusal_reason() {
            return Some(reason);
        }
        if self.has_children {
            return Some(
                "device already has a partition table; wipe it out-of-band first if you really \
                 intend to reuse it — this API only formats a genuinely empty disk"
                    .into(),
            );
        }
        if let Some(fstype) = &self.fstype {
            return Some(format!("device already has a filesystem signature: {fstype}"));
        }
        if let Some(pttype) = &self.pttype {
            return Some(format!("device already has a partition table: {pttype}"));
        }
        if !self.wipefs_signatures.is_empty() {
            return Some(format!(
                "residual filesystem/RAID/LVM signature detected: {}",
                self.wipefs_signatures.join(", ")
            ));
        }
        None
    }

    /// The recheck to run immediately after a successful `wipe_device` call — like
    /// `refusal_reason`, except it tolerates a lingering `has_children`. Confirmed live: `wipefs
    /// -a` genuinely clears the on-disk partition-table/filesystem signature (this device's
    /// `fstype`/`pttype` read back `None` and a follow-up `wipefs -n` shows nothing), but the
    /// kernel's already-parsed partition view (lsblk's `children`) can still list the old
    /// partitions — `blockdev --rereadpt` is best-effort, not guaranteed to clear it, and that
    /// residue no longer reflects real on-disk data. Every other check (including a lingering
    /// filesystem/partition-table *signature*, which would mean the wipe didn't actually work)
    /// still applies unconditionally.
    pub fn refusal_reason_after_wipe(&self) -> Option<String> {
        if let Some(reason) = self.hard_refusal_reason() {
            return Some(reason);
        }
        if let Some(fstype) = &self.fstype {
            return Some(format!(
                "device still has a filesystem signature after wipe: {fstype}"
            ));
        }
        if let Some(pttype) = &self.pttype {
            return Some(format!(
                "device still has a partition-table signature after wipe: {pttype}"
            ));
        }
        if !self.wipefs_signatures.is_empty() {
            return Some(format!(
                "residual filesystem/RAID/LVM signature detected after wipe: {}",
                self.wipefs_signatures.join(", ")
            ));
        }
        None
    }
}

fn basename(device_path: &str) -> &str {
    device_path.rsplit('/').next().unwrap_or(device_path)
}

/// The conventional first-disk names `atlas_common::device::validate_raw_device_path` always
/// refuses regardless of live host state — duplicated here as a minimal, dependency-free check
/// (rather than pulling in atlas-common) so this crate's own root/boot detection has a fallback
/// that still works when the dynamic, mount-namespace-dependent check above is blind (see its
/// comment). Keep in sync with that function's static rule if it ever changes.
fn is_conventional_first_disk(basename: &str) -> bool {
    matches!(basename, "sda" | "vda" | "nvme0n1")
}

/// Resolve a mounted mountpoint's source device (`findmnt -n -o SOURCE <mountpoint>`) down to its
/// parent whole-disk basename (`lsblk -no pkname <source>`, falling back to the source's own
/// basename when it has no parent, i.e. it's already a whole disk).
async fn resolve_mounted_disk_basename(mountpoint: &str) -> Option<String> {
    let (ok, source, _) = run("findmnt", &["-n", "-o", "SOURCE", mountpoint])
        .await
        .ok()?;
    if !ok || source.is_empty() {
        return None;
    }
    let (ok, pkname, _) = run("lsblk", &["-no", "PKNAME", &source]).await.ok()?;
    if ok && !pkname.trim().is_empty() {
        Some(pkname.trim().to_string())
    } else {
        Some(basename(&source).to_string())
    }
}

/// Same goal as `resolve_mounted_disk_basename` (find the whole-disk basename backing a
/// mountpoint), but reads the *host's* real mount table via a hostPath-mounted
/// `/proc/1/mountinfo` (path given by `ATLAS_HOST_MOUNTINFO_PATH`) instead of `findmnt`, which
/// only ever sees the calling *container's* own mount namespace. Closes the gap documented on
/// `inspect_device`'s step 4 below: on a containerized deploy without this env var set, the plain
/// `findmnt`-based check is blind to the host's actual root/boot disk (confirmed live: `/dev/sda`
/// wasn't flagged). A no-op (`None`) when the env var is unset — e.g. `make run` local dev, or
/// the gateway running as a bare process directly on the host, where the plain check already
/// works and this hostPath mount doesn't exist.
///
/// mountinfo's fields are space-separated; field 3 (0-indexed 2) is `major:minor`, field 5
/// (0-indexed 4) is the mountpoint — see proc(5). `major:minor` is resolved to a device name via
/// `/sys/dev/block/<maj>:<min>`, a symlink whose target's parent directory name is the whole
/// disk's basename (e.g. `.../block/sda/sda2` → `sda`) — sysfs block-device topology isn't
/// mount-namespace-scoped, so this works from inside the container same as it would on the host.
async fn resolve_host_mounted_disk_basename(mountpoint: &str) -> Option<String> {
    let path = std::env::var("ATLAS_HOST_MOUNTINFO_PATH").ok()?;
    let content = tokio::fs::read_to_string(&path).await.ok()?;
    let maj_min = content.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(' ').collect();
        (fields.len() > 4 && fields[4] == mountpoint).then(|| fields[2])
    })?;
    let target = tokio::fs::read_link(format!("/sys/dev/block/{maj_min}"))
        .await
        .ok()?;
    let parent_dir = target.parent()?.file_name()?.to_str()?;
    Some(parent_dir.to_string())
}

/// Runs every read-only safety probe against `device_path`. Never mutates anything.
pub async fn inspect_device(device_path: &str) -> Result<DeviceCheck, DriverError> {
    let mut check = DeviceCheck::default();

    // 1. lsblk: whole-disk shape, existing fs/partition signatures, read-only flag.
    let (ok, stdout, stderr) = run(
        "lsblk",
        &["-J", "-b", "-o", "NAME,TYPE,MOUNTPOINT,FSTYPE,PTTYPE,RO", device_path],
    )
    .await?;
    if !ok {
        // Not found / not a block device — the safe, honest answer is "does not exist", not an
        // unreachable-backend error, so a bad operator-supplied path surfaces as a normal refusal.
        check.exists = false;
        return Ok(check);
    }
    check.exists = true;
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|e| DriverError::Parse(format!("lsblk {device_path}: {e} (stderr: {stderr})")))?;
    if let Some(dev) = parsed
        .get("blockdevices")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
    {
        check.is_disk = dev.get("type").and_then(|v| v.as_str()) == Some("disk");
        // Different util-linux versions emit `ro` as a JSON boolean or as a "0"/"1" string.
        check.read_only = match dev.get("ro") {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(serde_json::Value::String(s)) => s == "1",
            _ => false,
        };
        check.has_children = dev
            .get("children")
            .and_then(|c| c.as_array())
            .is_some_and(|a| !a.is_empty());
        check.fstype = dev
            .get("fstype")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        check.pttype = dev
            .get("pttype")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        check.mounted_at = dev
            .get("mountpoint")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }

    // 2. findmnt: an independent check that the device (not just its lsblk-reported mountpoint
    // field) isn't mounted anywhere — a device can be a mount source without lsblk's own
    // `mountpoint` column reflecting it in every util-linux version.
    if check.mounted_at.is_none() {
        let (ok, target, _) = run("findmnt", &["-n", "-o", "TARGET", "--source", device_path]).await?;
        if ok && !target.is_empty() {
            check.mounted_at = Some(target);
        }
    }

    // 3. wipefs (no -a: detection only, never wipes) — a second, independent libblkid probe
    // catching residual signatures lsblk under-reports (e.g. stale mdraid superblocks).
    let (_, stdout, _) = run("wipefs", &["-n", device_path]).await?;
    check.wipefs_signatures = stdout
        .lines()
        .skip(1) // header row
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect();

    // 4. Dynamic root/boot-disk refusal: resolve the host's actual mounted root/boot devices to
    // their parent whole-disk basenames and compare against this device's own basename. In
    // principle stronger than any static allow-list heuristic, since the real boot disk isn't
    // always the first letter/index (cloud images, unusual partitioning) — BUT `findmnt` alone is
    // blind when the gateway runs in a container without visibility into the *host's* mount
    // namespace (e.g. the k8s lab deploy: `findmnt -n -o SOURCE /` inside the container resolves
    // the container's own rootfs, not the host's — confirmed live: `/dev/sda`, this host's actual
    // root/boot disk, was not flagged by `findmnt` alone). `resolve_host_mounted_disk_basename`
    // closes that gap when `ATLAS_HOST_MOUNTINFO_PATH` is set (a hostPath-mounted
    // `/proc/1/mountinfo`); `is_conventional_first_disk` below is the last-resort static fallback
    // when neither dynamic check can see the host's real mounts. `validate_raw_device_path`
    // (atlas-common) enforces that same static rule independently at the route/dispatch layer
    // regardless of what `DeviceCheck` reports.
    let target_basename = basename(device_path);
    for mountpoint in ["/", "/boot", "/boot/efi"] {
        let disk = resolve_mounted_disk_basename(mountpoint).await;
        let host_disk = resolve_host_mounted_disk_basename(mountpoint).await;
        if disk.as_deref() == Some(target_basename) || host_disk.as_deref() == Some(target_basename)
        {
            check.is_root_or_boot_disk = true;
            break;
        }
    }
    if is_conventional_first_disk(target_basename) {
        check.is_root_or_boot_disk = true;
    }

    // 5. Already a zpool member? Cross-check both the plain device path and its `/dev/disk/by-id`
    // aliases, since ZFS commonly reports vdevs by their by-id name, not `/dev/sdX`. No shell
    // pipeline here (arg-arrays only, per PDF §17.3) — `ls -la`'s own output is parsed in Rust.
    let (_, status_stdout, _) = run("zpool", &["status", "-P"]).await?;
    if !status_stdout.is_empty() {
        let (_, by_id_listing, _) = run("ls", &["-la", "/dev/disk/by-id/"]).await.unwrap_or_default();
        // Each symlink line looks like "lrwxrwxrwx ... ata-XXXXX -> ../../sdb" — keep only the
        // link *name* (before " -> "), for entries whose target resolves to this device.
        let by_id_aliases: Vec<String> = by_id_listing
            .lines()
            .filter(|l| l.ends_with(&format!("/{target_basename}")))
            .filter_map(|l| l.split(" -> ").next())
            .filter_map(|before_arrow| before_arrow.split_whitespace().last())
            .map(|name| format!("/dev/disk/by-id/{name}"))
            .collect();
        let by_id_aliases: Vec<&str> = by_id_aliases.iter().map(String::as_str).collect();
        check.member_of_zpool =
            find_zpool_member(&status_stdout, device_path, target_basename, &by_id_aliases);
    }

    Ok(check)
}

/// One row of `list_whole_disks`'s result — the read-only "what's out there" listing the Disks UI
/// renders as a picker instead of a blind device-path text field. Deliberately not `DeviceCheck`:
/// this describes N devices found by one bulk scan, not the single-device pass/fail decision
/// `inspect_device`/`refusal_reason` make right before a mutating command runs — that check is
/// re-run, authoritatively, against the chosen device at that point; this listing is advisory.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BlockDevice {
    pub path: String,
    pub size_bytes: u64,
    pub read_only: bool,
    pub has_children: bool,
    pub fstype: Option<String>,
    pub pttype: Option<String>,
    pub mounted_at: Option<String>,
    pub wipefs_signatures: Vec<String>,
    pub member_of_zpool: Option<String>,
    pub is_root_or_boot_disk: bool,
}

impl BlockDevice {
    /// A single coarse status label for the UI to badge/sort by — the fields above carry the
    /// actual detail. Ordered the same way `DeviceCheck::refusal_reason` is (root/boot first).
    pub fn status(&self) -> &'static str {
        if self.is_root_or_boot_disk {
            "root_or_boot"
        } else if self.read_only {
            "read_only"
        } else if self.mounted_at.is_some() {
            "mounted"
        } else if self.member_of_zpool.is_some() {
            "zpool_member"
        } else if self.has_children
            || self.fstype.is_some()
            || self.pttype.is_some()
            || !self.wipefs_signatures.is_empty()
        {
            "has_data"
        } else {
            "empty"
        }
    }
}

/// Bulk-scans every whole disk on the local host (partitions/loop/optical devices excluded) — the
/// same signals `inspect_device` gathers for one device, run once for all of them, so the Disks UI
/// can show a picker instead of a blind device-path text field.
pub async fn list_whole_disks() -> Result<Vec<BlockDevice>, DriverError> {
    let (ok, stdout, stderr) = run(
        "lsblk",
        &["-J", "-b", "-o", "NAME,SIZE,TYPE,MOUNTPOINT,FSTYPE,PTTYPE,RO"],
    )
    .await?;
    if !ok {
        return Err(DriverError::Unreachable(format!("lsblk: {stderr}")));
    }
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|e| DriverError::Parse(format!("lsblk: {e} (stderr: {stderr})")))?;
    let devices = parsed
        .get("blockdevices")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();

    // Root/boot-disk resolution done once, matched by basename against every listed disk — see
    // `inspect_device`'s step 4 for why this checks both the container's own `findmnt` view and,
    // when set, the host's real mount table via `ATLAS_HOST_MOUNTINFO_PATH`.
    let mut root_boot_basenames = std::collections::HashSet::new();
    for mountpoint in ["/", "/boot", "/boot/efi"] {
        if let Some(disk) = resolve_mounted_disk_basename(mountpoint).await {
            root_boot_basenames.insert(disk);
        }
        if let Some(disk) = resolve_host_mounted_disk_basename(mountpoint).await {
            root_boot_basenames.insert(disk);
        }
    }
    // zpool membership done once too (see `inspect_device`'s step 5); by-id aliases are re-filtered
    // per device below.
    let (_, status_stdout, _) = run("zpool", &["status", "-P"]).await?;
    let (_, by_id_listing, _) = run("ls", &["-la", "/dev/disk/by-id/"]).await.unwrap_or_default();

    let mut out = Vec::new();
    for dev in &devices {
        if dev.get("type").and_then(|v| v.as_str()) != Some("disk") {
            continue;
        }
        let Some(name) = dev.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
            continue;
        };
        let path = format!("/dev/{name}");
        let size_bytes = dev.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        // Unattached stubs (e.g. /dev/nbd0..15 report 0 bytes) can't hold a pool — not a candidate.
        if size_bytes == 0 {
            continue;
        }
        let read_only = match dev.get("ro") {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(serde_json::Value::String(s)) => s == "1",
            _ => false,
        };
        let has_children = dev
            .get("children")
            .and_then(|c| c.as_array())
            .is_some_and(|a| !a.is_empty());
        let fstype = dev
            .get("fstype")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let pttype = dev
            .get("pttype")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mounted_at = dev
            .get("mountpoint")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let (_, wipefs_stdout, _) = run("wipefs", &["-n", &path]).await.unwrap_or_default();
        let wipefs_signatures: Vec<String> = wipefs_stdout
            .lines()
            .skip(1)
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect();
        let member_of_zpool = if status_stdout.is_empty() {
            None
        } else {
            let by_id_aliases: Vec<String> = by_id_listing
                .lines()
                .filter(|l| l.ends_with(&format!("/{name}")))
                .filter_map(|l| l.split(" -> ").next())
                .filter_map(|before_arrow| before_arrow.split_whitespace().last())
                .map(|alias| format!("/dev/disk/by-id/{alias}"))
                .collect();
            let by_id_aliases: Vec<&str> = by_id_aliases.iter().map(String::as_str).collect();
            find_zpool_member(&status_stdout, &path, name, &by_id_aliases)
        };

        out.push(BlockDevice {
            path,
            size_bytes,
            read_only,
            has_children,
            fstype,
            pttype,
            mounted_at,
            wipefs_signatures,
            member_of_zpool,
            is_root_or_boot_disk: root_boot_basenames.contains(name)
                || is_conventional_first_disk(name),
        });
    }
    Ok(out)
}

/// Whether `vdev` (a name as printed in `zpool status -P`) is `device` itself or one of its
/// partitions. ZFS whole-disk pools are labeled with a GPT and list the *partition* as the vdev
/// (confirmed live: a pool created on /dev/sdb shows `/dev/sdb1`), so an exact-path comparison
/// alone never recognizes the disk as a pool member.
///
/// Partition naming: names ending in a digit (nvme1n1, mmcblk0) separate the number with `p`
/// (nvme1n1p1); others just append it (sdb1). Requiring that exact shape keeps `/dev/nvme1n11`
/// (a different namespace) and `/dev/sdbb` (a different disk) from matching `/dev/nvme1n1` / `/dev/sdb`.
fn is_device_or_partition(vdev: &str, device: &str) -> bool {
    if vdev == device {
        return true;
    }
    let Some(rest) = vdev.strip_prefix(device) else {
        return false;
    };
    let digits = if device.ends_with(|c: char| c.is_ascii_digit()) {
        match rest.strip_prefix('p') {
            Some(d) => d,
            None => return false,
        }
    } else {
        rest
    };
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Scan `zpool status -P` output for a pool that lists this device — or one of its partitions, or
/// a `/dev/disk/by-id` alias of either — as a vdev. Returns the owning pool's name if found.
fn find_zpool_member(
    status: &str,
    device_path: &str,
    basename: &str,
    by_id_aliases: &[&str],
) -> Option<String> {
    let mut current_pool: Option<&str> = None;
    for line in status.lines() {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix("pool: ") {
            current_pool = Some(name.trim());
            continue;
        }
        let Some(first_field) = trimmed.split_whitespace().next() else {
            continue;
        };
        // by-id partitions are `<alias>-part<N>`.
        let matches_by_id = by_id_aliases.iter().any(|alias| {
            first_field == *alias
                || first_field
                    .strip_prefix(alias)
                    .and_then(|r| r.strip_prefix("-part"))
                    .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        });
        let matches_direct = is_device_or_partition(first_field, device_path)
            // Bare (non -P) names, for robustness if the caller's status output isn't full-path.
            || is_device_or_partition(first_field, basename);
        if (matches_direct || matches_by_id) && current_pool.is_some() {
            return current_pool.map(str::to_string);
        }
    }
    None
}

/// Clears a residual partition table/filesystem/RAID/LVM signature off `device_path` so a
/// subsequent `inspect_device` call no longer reports `has_children`/`fstype`/`pttype`/
/// `wipefs_signatures`. Only reached when the operator has explicitly opted into `wipe_existing`
/// (see `JobSpec::ZfsPoolCreateFromDevice`) — `hard_refusal_reason` is still checked first and is
/// never bypassed by this, so this never runs against the root/boot disk, a mounted device, or
/// anything that isn't a whole disk.
pub async fn wipe_device(device_path: &str) -> Result<(), DriverError> {
    let (ok, _stdout, stderr) = run("wipefs", &["-a", device_path]).await?;
    if !ok {
        return Err(DriverError::Backend(format!(
            "wipefs -a {device_path}: {stderr}"
        )));
    }
    // Best-effort: `wipefs -a` erases the on-disk partition-table signature, but the kernel's
    // already-parsed partition entries (lsblk's `children`) can persist until it re-reads the
    // partition table — ignore failure here, `zpool create` will surface any real problem itself.
    let _ = run("blockdev", &["--rereadpt", device_path]).await;
    Ok(())
}

/// `zpool create <pool_name> <device_path>` — **never** passes `-f` (force): that flag exists
/// specifically to override zpool's own built-in "this looks like it's in use" refusal, which is
/// the safety net underneath everything `inspect_device` already checked.
pub async fn zpool_create(pool_name: &str, device_path: &str) -> Result<(), DriverError> {
    // `zpool create` on a whole disk writes a new GPT (BLKPG_ADD_PARTITION) and then immediately
    // reopens the partition device node it just asked the kernel to create — a real, observed race
    // when the gateway runs in a container whose /dev is a hostPath bind mount: the kernel creates
    // the device (confirmed via the host's own dmesg) before devtmpfs has propagated the new node
    // into the container's view, and zpool has no retry/settle wait of its own for this. `zpool
    // create` never rewrites/re-labels anything on a retry here (each attempt is against the same
    // freshly-written, still-valid label) — never passes `-f`, same rule as the first attempt.
    const MAX_ATTEMPTS: u32 = 5;
    let mut last_err = String::new();
    for attempt in 1..=MAX_ATTEMPTS {
        let (ok, _stdout, stderr) = run("zpool", &["create", pool_name, device_path]).await?;
        if ok {
            return Ok(());
        }
        last_err = stderr;
        if attempt < MAX_ATTEMPTS && last_err.contains("failed to detect device partitions") {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            continue;
        }
        break;
    }
    Err(DriverError::Backend(format!(
        "zpool create {pool_name} {device_path}: {last_err}"
    )))
}

/// Names of every imported zpool on this host (`zpool list -H -o name`).
pub async fn list_zpool_names() -> Result<Vec<String>, DriverError> {
    let (ok, stdout, stderr) = run("zpool", &["list", "-H", "-o", "name"]).await?;
    if !ok {
        return Err(DriverError::Backend(format!("zpool list: {stderr}")));
    }
    Ok(stdout.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
}

/// Every dataset/zvol below (and including) `pool_name` (`zfs list -H -r -o name`).
pub async fn list_pool_datasets(pool_name: &str) -> Result<Vec<String>, DriverError> {
    let (ok, stdout, stderr) = run("zfs", &["list", "-H", "-r", "-o", "name", pool_name]).await?;
    if !ok {
        return Err(DriverError::Backend(format!("zfs list {pool_name}: {stderr}")));
    }
    Ok(stdout.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
}

/// Datasets that would be destroyed along with the pool, i.e. everything except the root
/// dataset itself. Non-empty means the pool holds real data and destroy must be refused.
pub fn non_root_datasets<'a>(pool_name: &str, datasets: &'a [String]) -> Vec<&'a String> {
    datasets.iter().filter(|d| d.as_str() != pool_name).collect()
}

/// `zpool destroy <pool_name>` — **never** passes `-f`: without it zpool itself refuses a pool
/// with mounted, in-use datasets ("pool is busy"), an independent safety net under Atlas's own
/// dataset check. The member disk keeps its ZFS labels afterwards and is then offered by the
/// Disks picker as a wipeable `zfs_member` device.
pub async fn zpool_destroy(pool_name: &str) -> Result<(), DriverError> {
    let (ok, _stdout, stderr) = run("zpool", &["destroy", pool_name]).await?;
    if ok {
        Ok(())
    } else {
        Err(DriverError::Backend(format!("zpool destroy {pool_name}: {stderr}")))
    }
}

/// The synthesized inventory pool/cluster ids this device-provision path writes, matching
/// `RealZfsDriver`/`FakeZfsDriver`'s own id-derivation scheme exactly, so a later real discovery
/// pass (once the pool name is added to `ATLAS_ZFS_POOLS`) upserts onto the same row.
pub fn pool_id(pool_name: &str) -> String {
    format!("pool_zfs_{}", sanitize(pool_name))
}

pub fn cluster_id(host: &str) -> String {
    format!("cls_zfs_{}", sanitize(host))
}

/// The id a discovery pass gives the pool's own root dataset — `FakeZfsDriver::list_volumes`
/// derives it from the pool name directly, and `RealZfsDriver::list_volumes` derives it from the
/// dataset name via `zfs list -r <pool>`, whose first/root entry is always named exactly
/// `pool_name` — so both modes agree on this id for the pool's root dataset.
pub fn root_volume_id(pool_name: &str) -> String {
    format!("vol_zfs_{}", sanitize(pool_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_root_datasets_ignores_only_the_root() {
        let all = vec!["tank0".to_string(), "tank0/data".to_string(), "tank00".to_string()];
        let extra = non_root_datasets("tank0", &all);
        assert_eq!(extra, vec![&all[1], &all[2]]);
        assert!(non_root_datasets("tank0", &all[..1]).is_empty());
    }

    #[test]
    fn refuses_root_disk_before_anything_else() {
        let check = DeviceCheck {
            exists: true,
            is_root_or_boot_disk: true,
            read_only: true, // also true, but root/boot must win regardless of order
            ..Default::default()
        };
        assert!(check.refusal_reason().unwrap().contains("root or boot"));
    }

    #[test]
    fn refuses_missing_device() {
        let check = DeviceCheck::default();
        assert!(check.refusal_reason().unwrap().contains("no such"));
    }

    #[test]
    fn refuses_partition() {
        let check = DeviceCheck {
            exists: true,
            is_disk: false,
            ..Default::default()
        };
        assert!(check.refusal_reason().unwrap().contains("whole disk"));
    }

    #[test]
    fn refuses_existing_filesystem() {
        let check = DeviceCheck {
            exists: true,
            is_disk: true,
            fstype: Some("ext4".into()),
            ..Default::default()
        };
        assert!(check.refusal_reason().unwrap().contains("ext4"));
    }

    #[test]
    fn refuses_mounted_device() {
        let check = DeviceCheck {
            exists: true,
            is_disk: true,
            mounted_at: Some("/mnt/data".into()),
            ..Default::default()
        };
        assert!(check.refusal_reason().unwrap().contains("/mnt/data"));
    }

    #[test]
    fn hard_refusal_reason_ignores_wipeable_data() {
        // A stale partition table + filesystem signature — exactly what `wipe_existing` is for —
        // must NOT show up in `hard_refusal_reason`, only in the full `refusal_reason`.
        let check = DeviceCheck {
            exists: true,
            is_disk: true,
            has_children: true,
            fstype: Some("ceph_bluestore".into()),
            pttype: Some("gpt".into()),
            wipefs_signatures: vec!["gpt".into()],
            ..Default::default()
        };
        assert!(check.hard_refusal_reason().is_none());
        assert!(check.refusal_reason().is_some());
    }

    #[test]
    fn imported_zpool_membership_is_a_hard_never_overridable_refusal() {
        // A disk in a *currently imported* pool must be refused even on the wipe_existing path
        // (which only checks hard_refusal_reason before wiping) — wiping it would corrupt a live pool.
        let live_member = DeviceCheck {
            exists: true,
            is_disk: true,
            member_of_zpool: Some("tank0".into()),
            ..Default::default()
        };
        assert!(live_member.hard_refusal_reason().unwrap().contains("tank0"));
        assert!(live_member.refusal_reason().unwrap().contains("tank0"));
        assert!(live_member
            .refusal_reason_after_wipe()
            .unwrap()
            .contains("tank0"));
    }

    #[test]
    fn block_device_status_reports_zpool_member_before_has_data() {
        let d = BlockDevice {
            path: "/dev/sdb".into(),
            size_bytes: 1_000_000,
            has_children: true,
            member_of_zpool: Some("tank0".into()),
            ..Default::default()
        };
        assert_eq!(d.status(), "zpool_member");
    }

    #[test]
    fn refusal_reason_after_wipe_tolerates_stale_partition_table_only() {
        // Exactly the state found live: wipefs -a genuinely cleared the signature (fstype/pttype/
        // wipefs_signatures all empty), but lsblk's kernel-cached `children` is still stale.
        let stale_only = DeviceCheck {
            exists: true,
            is_disk: true,
            has_children: true,
            ..Default::default()
        };
        assert!(stale_only.refusal_reason_after_wipe().is_none());
        // ... but the plain (pre-wipe) refusal_reason still refuses it, as a sanity check that
        // this really is a distinct, more permissive check.
        assert!(stale_only.refusal_reason().is_some());
    }

    #[test]
    fn refusal_reason_after_wipe_still_catches_a_real_leftover_signature() {
        let still_dirty = DeviceCheck {
            exists: true,
            is_disk: true,
            has_children: true,
            fstype: Some("ext4".into()),
            ..Default::default()
        };
        assert!(still_dirty
            .refusal_reason_after_wipe()
            .unwrap()
            .contains("ext4"));
    }

    #[test]
    fn hard_refusal_reason_still_blocks_mounted_and_root_disk() {
        let mounted = DeviceCheck {
            exists: true,
            is_disk: true,
            mounted_at: Some("/mnt/data".into()),
            ..Default::default()
        };
        assert!(mounted.hard_refusal_reason().unwrap().contains("/mnt/data"));

        let root_disk = DeviceCheck {
            exists: true,
            is_disk: true,
            is_root_or_boot_disk: true,
            ..Default::default()
        };
        assert!(root_disk.hard_refusal_reason().unwrap().contains("root or boot"));
    }

    #[test]
    fn refuses_existing_zpool_member() {
        let check = DeviceCheck {
            exists: true,
            is_disk: true,
            member_of_zpool: Some("tank".into()),
            ..Default::default()
        };
        assert!(check.refusal_reason().unwrap().contains("tank"));
    }

    #[test]
    fn accepts_a_genuinely_empty_disk() {
        let check = DeviceCheck {
            exists: true,
            is_disk: true,
            ..Default::default()
        };
        assert!(check.refusal_reason().is_none());
    }

    #[test]
    fn pool_and_cluster_ids_match_the_driver_scheme() {
        assert_eq!(pool_id("tank"), "pool_zfs_tank");
        assert_eq!(cluster_id("zfs01.zyvor.lab"), "cls_zfs_zfs01_zyvor_lab");
    }

    /// The exact `zpool status -P` shape captured live for a whole-disk pool on /dev/sdb.
    const LIVE_STATUS: &str = "  pool: tank0\n state: ONLINE\nconfig:\n\n\tNAME         STATE     READ WRITE CKSUM\n\ttank0        ONLINE       0     0     0\n\t  /dev/sdb1  ONLINE       0     0     0\n\nerrors: No known data errors\n";

    #[test]
    fn whole_disk_pool_lists_the_partition_and_still_counts_as_a_member() {
        // Regression for a bug found live: the vdev is /dev/sdb1, not /dev/sdb.
        assert_eq!(
            find_zpool_member(LIVE_STATUS, "/dev/sdb", "sdb", &[]),
            Some("tank0".into())
        );
    }

    #[test]
    fn zpool_membership_does_not_bleed_onto_other_disks() {
        assert_eq!(find_zpool_member(LIVE_STATUS, "/dev/sdc", "sdc", &[]), None);
        // /dev/sdbb is a different disk whose name merely starts with "sdb".
        assert_eq!(find_zpool_member(LIVE_STATUS, "/dev/sdbb", "sdbb", &[]), None);
    }

    #[test]
    fn partition_name_shapes() {
        assert!(is_device_or_partition("/dev/sdb", "/dev/sdb"));
        assert!(is_device_or_partition("/dev/sdb1", "/dev/sdb"));
        assert!(is_device_or_partition("/dev/sdb12", "/dev/sdb"));
        assert!(!is_device_or_partition("/dev/sdbb", "/dev/sdb"));
        assert!(!is_device_or_partition("/dev/sdb1x", "/dev/sdb"));
        // Names ending in a digit use the `p` separator; a bare digit suffix is another device.
        assert!(is_device_or_partition("/dev/nvme1n1p1", "/dev/nvme1n1"));
        assert!(!is_device_or_partition("/dev/nvme1n11", "/dev/nvme1n1"));
        assert!(!is_device_or_partition("/dev/nvme1n1p", "/dev/nvme1n1"));
    }

    #[test]
    fn by_id_partition_alias_counts_as_a_member() {
        let status = "  pool: tank1\nconfig:\n\tNAME STATE\n\t  /dev/disk/by-id/ata-WDC_X-part1  ONLINE\n";
        assert_eq!(
            find_zpool_member(status, "/dev/sdd", "sdd", &["/dev/disk/by-id/ata-WDC_X"]),
            Some("tank1".into())
        );
    }

    #[test]
    fn conventional_first_disk_fallback_covers_the_container_blind_spot() {
        // This is the static backstop for exactly the bug found live: inside a container without
        // visibility into the host's mount namespace, the dynamic findmnt-based check in
        // inspect_device/list_whole_disks can't see that e.g. /dev/sda is actually mounted at the
        // host's "/" — /dev/sda showed up in the Disks UI picker labeled "has data — wipeable"
        // before this fallback existed.
        assert!(is_conventional_first_disk("sda"));
        assert!(is_conventional_first_disk("vda"));
        assert!(is_conventional_first_disk("nvme0n1"));
        assert!(!is_conventional_first_disk("sdb"));
        assert!(!is_conventional_first_disk("nvme1n1"));
    }
}
