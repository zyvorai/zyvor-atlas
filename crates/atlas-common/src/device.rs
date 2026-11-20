// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Validation for raw block-device paths accepted from an operator (disk-provisioning APIs).
//!
//! This is a cheap, hand-rolled first gate — not the authoritative safety check. It only rejects
//! syntactically-obvious danger (a partition, a common first-disk letter, shell metacharacters);
//! the real "is this device actually safe to format" check (unmounted, no filesystem/partition
//! table, not already claimed) happens against the live device itself, closer to where the
//! formatting command runs.

/// Whole-disk device paths this feature is willing to touch at all. Deliberately excludes the
/// conventional first-disk letter/index for each bus (`sda`, `vda`, `nvme0n1`) as a static
/// heuristic — NOT the authoritative root-disk check, which must resolve the live host's actual
/// mounted root/boot device before formatting anything.
pub fn validate_raw_device_path(path: &str) -> Result<(), String> {
    let Some(basename) = path.strip_prefix("/dev/") else {
        return Err(format!("device path {path:?} must start with /dev/"));
    };
    if basename.is_empty()
        || !basename
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "device path {path:?} has an invalid or unsafe basename"
        ));
    }

    if let Some(rest) = basename.strip_prefix("sd").or_else(|| basename.strip_prefix("vd")) {
        return validate_sd_or_vd(basename, rest);
    }
    if let Some(rest) = basename.strip_prefix("nvme") {
        return validate_nvme(basename, rest);
    }
    Err(format!(
        "device path {path:?} is not a recognized whole-disk name (expected sdX, vdX, or nvmeXnY)"
    ))
}

fn validate_sd_or_vd(basename: &str, rest: &str) -> Result<(), String> {
    // Whole disk only: exactly one or more lowercase letters, no trailing digits (a trailing
    // digit means a partition, e.g. "sdb1").
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_lowercase()) {
        return Err(format!(
            "{basename:?} is not a whole-disk device (partitions like sdb1 are refused)"
        ));
    }
    // Static first-disk heuristic: refuse the conventional boot/root disk letter outright. This
    // is a cheap first filter only — the real root/boot-disk refusal is dynamic (resolves the
    // live host's actual mounted root/boot device), done at the point of use.
    if rest == "a" {
        return Err(format!(
            "{basename:?} is the conventional first disk (commonly the boot/root disk) — refused"
        ));
    }
    Ok(())
}

fn validate_nvme(basename: &str, rest: &str) -> Result<(), String> {
    // Expect "<controller digits>n<namespace digits>", no trailing "p<N>" partition suffix.
    let Some((controller, namespace)) = rest.split_once('n') else {
        return Err(format!(
            "{basename:?} is not a recognized nvme whole-disk name (expected nvmeXnY)"
        ));
    };
    if controller.is_empty() || !controller.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{basename:?} has an invalid nvme controller index"));
    }
    if namespace.is_empty() || !namespace.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "{basename:?} is not a whole-disk device (partitions like nvme1n1p1 are refused)"
        ));
    }
    if controller == "0" && namespace == "1" {
        return Err(format!(
            "{basename:?} is the conventional first NVMe disk (commonly the boot/root disk) — refused"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_whole_disk_paths() {
        assert!(validate_raw_device_path("/dev/sdb").is_ok());
        assert!(validate_raw_device_path("/dev/sdz").is_ok());
        assert!(validate_raw_device_path("/dev/vdc").is_ok());
        assert!(validate_raw_device_path("/dev/nvme1n1").is_ok());
        assert!(validate_raw_device_path("/dev/nvme12n3").is_ok());
    }

    #[test]
    fn rejects_conventional_first_disks() {
        assert!(validate_raw_device_path("/dev/sda").is_err());
        assert!(validate_raw_device_path("/dev/vda").is_err());
        assert!(validate_raw_device_path("/dev/nvme0n1").is_err());
    }

    #[test]
    fn rejects_partitions() {
        assert!(validate_raw_device_path("/dev/sdb1").is_err());
        assert!(validate_raw_device_path("/dev/vdb2").is_err());
        assert!(validate_raw_device_path("/dev/nvme1n1p1").is_err());
    }

    #[test]
    fn rejects_malformed_or_unsafe_input() {
        assert!(validate_raw_device_path("sdb").is_err());
        assert!(validate_raw_device_path("/dev/").is_err());
        assert!(validate_raw_device_path("/dev/../etc/passwd").is_err());
        assert!(validate_raw_device_path("/dev/sdb; rm -rf /").is_err());
        assert!(validate_raw_device_path("/dev/sdb`whoami`").is_err());
        assert!(validate_raw_device_path("/dev/loop0").is_err());
        assert!(validate_raw_device_path("/dev/sr0").is_err());
    }
}
