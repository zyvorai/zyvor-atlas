// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! A read-only GPU / GPUDirect Storage (GDS) capability probe.
//!
//! **UNVERIFIED on real GPU hardware.** This module was written and tested against fixture
//! directory trees only; the development host has no GPU, no CUDA and no `nvidia_fs`. No GPU I/O
//! path exists in Atlas, and the probe performs none: it reads a handful of files and reports what
//! they say. It never links CUDA or cuFile.
//!
//! What it reads, relative to a root directory (`/` in production, a fixture tree in tests):
//! `proc/modules`, `proc/driver/nvidia-fs/`, `proc/driver/nvidia/version`,
//! `proc/sys/kernel/osrelease`, `dev/nvidia*` and `etc/cufile.json`.
//!
//! The reported status is:
//! - [`GpuIoStatus::None`]: no GPU the driver exposes (no `dev/nvidia<N>` node, or the `nvidia`
//!   module is not loaded).
//! - [`GpuIoStatus::Compat`]: a GPU is present but GDS is not corroborated, so cuFile would use its
//!   POSIX bounce-buffer ("compat") path, or refuse when `allow_compat_mode` is false.
//! - [`GpuIoStatus::Gds`]: every condition in [`probe`] is corroborated by a file. A file that is
//!   missing, unreadable or malformed counts as "not corroborated", never as a pass.
//!
//! `Gds` means "the machine looks capable", not "GDS works": that would need a real cuFile run.
//!
//! The `cufile` cargo feature compiles `cufile`, a dlopen probe. Its report is attached as
//! `cufile` on [`GpuReport`] only in that build. It does not change `status`.

use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::raw::DeviceBackend;

/// Stated in every report so nobody reads a probe as a measurement.
pub const PROBE_NOTE: &str = "probe only, no I/O was performed; unverified on real GPU hardware";

/// Minimum Linux kernel for CUDA 12.8+ local-NVMe GDS through PCI P2PDMA (per NVIDIA's docs).
pub const MIN_KERNEL: (u32, u32) = (6, 2);
/// Minimum NVIDIA open GPU kernel module ("OpenRM") major version for the same path.
pub const MIN_OPENRM_MAJOR: u32 = 570;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuIoStatus {
    Gds,
    Compat,
    None,
}

/// What the probe saw. `None` means "not determinable from the files", which is never a pass.
#[derive(Debug, Clone, Serialize)]
pub struct GpuEvidence {
    pub nvidia_module: bool,
    pub nvidia_fs_module: bool,
    pub nvidia_peermem_module: bool,
    /// `proc/driver/nvidia-fs/` exists.
    pub nvidia_fs_proc: bool,
    /// `dev/nvidia<N>` nodes found (the control nodes `nvidiactl` etc. do not count).
    pub gpu_devices: Vec<String>,
    pub kernel_release: Option<String>,
    /// Kernel is at least [`MIN_KERNEL`]; `None` if it could not be read or parsed.
    pub kernel_ok: Option<bool>,
    /// The driver reports the open kernel module at [`MIN_OPENRM_MAJOR`] or later.
    pub open_rm_ok: Option<bool>,
    /// `properties.allow_compat_mode` from `etc/cufile.json`.
    pub allow_compat_mode: Option<bool>,
    /// `properties.force_compat_mode` from `etc/cufile.json`.
    pub force_compat_mode: Option<bool>,
    /// The data node's device backends, as `file` / `aligned` / `io_uring`.
    pub backends: Vec<&'static str>,
    /// At least one backend opens with `O_DIRECT` (only `io_uring` does).
    pub direct_io_backend: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuReport {
    pub status: GpuIoStatus,
    /// Why this status, and what could not be corroborated.
    pub reasons: Vec<String>,
    /// Always true: nothing but file reads happened.
    pub probe_only: bool,
    /// Always false: no part of this has run against a real GPU.
    pub verified_on_hardware: bool,
    pub note: &'static str,
    pub evidence: GpuEvidence,
    /// Present only when this crate is built with `--features cufile`.
    #[cfg(feature = "cufile")]
    pub cufile: crate::cufile::Probe,
}

fn backend_name(b: DeviceBackend) -> &'static str {
    match b {
        DeviceBackend::File => "file",
        DeviceBackend::Aligned => "aligned",
        DeviceBackend::IoUring => "io_uring",
    }
}

/// Reads `root/rel`, recording any error other than "not found" in `problems`.
fn read(root: &Path, rel: &str, problems: &mut Vec<String>) -> Option<String> {
    let p: PathBuf = root.join(rel);
    match fs::read_to_string(&p) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => {
            problems.push(format!("could not read /{rel}: {e}"));
            None
        }
    }
}

/// Drops `//` and `/* */` comments outside strings (the cuFile config ships with them).
fn strip_json_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let (mut in_str, mut esc) = (false, false);
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_str = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = '\0';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// `(allow_compat_mode, force_compat_mode)`; an `Err` is a reason string for a malformed file.
fn parse_cufile(text: &str) -> Result<(Option<bool>, Option<bool>), String> {
    let v: serde_json::Value = serde_json::from_str(&strip_json_comments(text))
        .map_err(|e| format!("/etc/cufile.json is not valid JSON: {e}"))?;
    let props = v
        .get("properties")
        .ok_or_else(|| "/etc/cufile.json has no \"properties\" object".to_string())?;
    let flag = |k: &str| match props.get(k) {
        None => Ok(None),
        Some(serde_json::Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("/etc/cufile.json properties.{k} is not a boolean")),
    };
    Ok((flag("allow_compat_mode")?, flag("force_compat_mode")?))
}

fn parse_kernel(release: &str) -> Option<(u32, u32)> {
    let mut it = release.trim().split(|c: char| !c.is_ascii_digit());
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    Some((major, minor))
}

/// `Some(true)` for "Open Kernel Module" at or above [`MIN_OPENRM_MAJOR`].
fn parse_open_rm(version: &str) -> Option<bool> {
    let line = version.lines().find(|l| l.contains("NVRM version"))?;
    let open = line.contains("Open Kernel Module");
    let major: u32 = line
        .split_whitespace()
        .find_map(|w| w.split('.').next().filter(|m| m.len() == 3)?.parse().ok())?;
    Some(open && major >= MIN_OPENRM_MAJOR)
}

/// Probes the machine under `root` for GPU and GDS capability. `backends` are the data node's
/// device backends (empty: none known). Reads files only; never performs I/O on a device.
pub fn probe(root: &Path, backends: &[DeviceBackend]) -> GpuReport {
    let mut reasons = Vec::new();
    let mut problems = Vec::new();

    let modules = read(root, "proc/modules", &mut problems);
    let has_module = |name: &str| {
        modules.as_deref().is_some_and(|m| {
            m.lines()
                .any(|l| l.split_whitespace().next() == Some(name))
        })
    };
    let (nvidia_module, nvidia_fs_module, nvidia_peermem_module) = (
        has_module("nvidia"),
        has_module("nvidia_fs"),
        has_module("nvidia_peermem"),
    );

    let nvidia_fs_proc = root.join("proc/driver/nvidia-fs").is_dir();

    let mut gpu_devices: Vec<String> = Vec::new();
    match fs::read_dir(root.join("dev")) {
        Ok(rd) => {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name
                    .strip_prefix("nvidia")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                {
                    gpu_devices.push(name);
                }
            }
            gpu_devices.sort();
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => problems.push(format!("could not list /dev: {e}")),
    }

    let kernel_release = read(root, "proc/sys/kernel/osrelease", &mut problems)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let kernel_ok = kernel_release
        .as_deref()
        .and_then(parse_kernel)
        .map(|k| k >= MIN_KERNEL);

    let open_rm_ok = read(root, "proc/driver/nvidia/version", &mut problems)
        .as_deref()
        .and_then(parse_open_rm);

    let (mut allow_compat_mode, mut force_compat_mode) = (None, None);
    if let Some(text) = read(root, "etc/cufile.json", &mut problems) {
        match parse_cufile(&text) {
            Ok((a, f)) => (allow_compat_mode, force_compat_mode) = (a, f),
            Err(e) => problems.push(e),
        }
    }

    let direct_io_backend = backends.contains(&DeviceBackend::IoUring);

    // None: no GPU this probe can see.
    let gpu_present = nvidia_module && !gpu_devices.is_empty();
    if !gpu_present {
        if !nvidia_module {
            reasons.push("the nvidia kernel module is not listed in /proc/modules".into());
        }
        if gpu_devices.is_empty() {
            reasons.push("no /dev/nvidia<N> device node".into());
        }
        reasons.extend(problems);
        return report(
            GpuIoStatus::None,
            reasons,
            GpuEvidence {
                nvidia_module,
                nvidia_fs_module,
                nvidia_peermem_module,
                nvidia_fs_proc,
                gpu_devices,
                kernel_release,
                kernel_ok,
                open_rm_ok,
                allow_compat_mode,
                force_compat_mode,
                backends: backends.iter().map(|b| backend_name(*b)).collect(),
                direct_io_backend,
            },
        );
    }

    // Each unmet condition is a reason the GDS path is not claimed.
    let mut gds_blockers: Vec<String> = Vec::new();
    if !nvidia_fs_module {
        gds_blockers.push("nvidia_fs is not listed in /proc/modules (cuFile would use compat mode)".into());
    }
    if !nvidia_fs_proc {
        gds_blockers.push("/proc/driver/nvidia-fs is absent, so nvidia_fs is not corroborated".into());
    }
    match kernel_ok {
        Some(true) => {}
        Some(false) => gds_blockers.push(format!(
            "kernel {} is older than {}.{} (needed for local NVMe P2PDMA)",
            kernel_release.as_deref().unwrap_or("?"),
            MIN_KERNEL.0,
            MIN_KERNEL.1
        )),
        None => gds_blockers.push("kernel version could not be determined".into()),
    }
    match open_rm_ok {
        Some(true) => {}
        Some(false) => gds_blockers.push(format!(
            "the NVIDIA driver is not the open kernel module at {MIN_OPENRM_MAJOR} or later"
        )),
        None => gds_blockers.push(
            "the NVIDIA driver flavour/version could not be determined from /proc/driver/nvidia/version".into(),
        ),
    }
    if !direct_io_backend {
        gds_blockers.push(
            "no io_uring (O_DIRECT) data-node backend; file and aligned backends do not use O_DIRECT, so GDS reads are not plausible"
                .into(),
        );
    }
    if force_compat_mode == Some(true) {
        gds_blockers.push("cufile.json sets force_compat_mode=true".into());
    }
    // The backing filesystem is not checked: the probe only reads files and cannot tell.
    reasons.push("filesystem of the backing devices was not checked".into());

    let status = if gds_blockers.is_empty() {
        reasons.push(
            "every probed condition is present; this indicates capability only, not that GDS works"
                .into(),
        );
        GpuIoStatus::Gds
    } else {
        reasons.extend(gds_blockers);
        if allow_compat_mode == Some(false) {
            reasons.push(
                "cufile.json sets allow_compat_mode=false: cuFile would fail rather than fall back"
                    .into(),
            );
        }
        GpuIoStatus::Compat
    };
    reasons.extend(problems);
    report(
        status,
        reasons,
        GpuEvidence {
            nvidia_module,
            nvidia_fs_module,
            nvidia_peermem_module,
            nvidia_fs_proc,
            gpu_devices,
            kernel_release,
            kernel_ok,
            open_rm_ok,
            allow_compat_mode,
            force_compat_mode,
            backends: backends.iter().map(|b| backend_name(*b)).collect(),
            direct_io_backend,
        },
    )
}

fn report(status: GpuIoStatus, reasons: Vec<String>, evidence: GpuEvidence) -> GpuReport {
    GpuReport {
        status,
        reasons,
        probe_only: true,
        verified_on_hardware: false,
        note: PROBE_NOTE,
        evidence,
        #[cfg(feature = "cufile")]
        cufile: crate::cufile::probe(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn put(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    const OPEN_570: &str =
        "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  570.86.15  Release Build\n";

    /// A tree that corroborates every GDS condition.
    fn gds_tree() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        put(r, "proc/modules", "nvidia 1 0 - Live 0x0\nnvidia_fs 1 0 - Live 0x0\nnvidia_peermem 1 0 - Live 0x0\n");
        put(r, "proc/driver/nvidia-fs/stats", "x\n");
        put(r, "proc/driver/nvidia/version", OPEN_570);
        put(r, "proc/sys/kernel/osrelease", "6.8.0-45-generic\n");
        put(r, "dev/nvidia0", "");
        put(r, "dev/nvidiactl", "");
        put(
            r,
            "etc/cufile.json",
            "{\n // comment with \"quotes\"\n \"properties\": { \"allow_compat_mode\": true, /* x */ \"force_compat_mode\": false }\n}\n",
        );
        t
    }

    const URING: &[DeviceBackend] = &[DeviceBackend::IoUring];

    fn has(r: &GpuReport, needle: &str) -> bool {
        r.reasons.iter().any(|s| s.contains(needle))
    }

    #[test]
    fn gds_capable_fixture_reports_gds_with_caveats() {
        let t = gds_tree();
        let r = probe(t.path(), URING);
        assert_eq!(r.status, GpuIoStatus::Gds, "{:?}", r.reasons);
        assert!(r.probe_only && !r.verified_on_hardware);
        assert!(r.note.contains("probe only, no I/O was performed"));
        assert_eq!(r.evidence.gpu_devices, ["nvidia0"]);
        assert_eq!(r.evidence.allow_compat_mode, Some(true));
        assert_eq!(r.evidence.force_compat_mode, Some(false));
        assert!(has(&r, "not that GDS works"));
        assert!(has(&r, "filesystem"));
    }

    #[test]
    fn missing_nvidia_fs_is_compat() {
        let t = gds_tree();
        put(t.path(), "proc/modules", "nvidia 1 0 - Live 0x0\n");
        fs::remove_dir_all(t.path().join("proc/driver/nvidia-fs")).unwrap();
        let r = probe(t.path(), URING);
        assert_eq!(r.status, GpuIoStatus::Compat);
        assert!(has(&r, "nvidia_fs is not listed"));
    }

    #[test]
    fn module_without_proc_dir_is_not_corroborated() {
        let t = gds_tree();
        fs::remove_dir_all(t.path().join("proc/driver/nvidia-fs")).unwrap();
        let r = probe(t.path(), URING);
        assert_eq!(r.status, GpuIoStatus::Compat);
        assert!(has(&r, "not corroborated"));
    }

    #[test]
    fn empty_tree_is_none() {
        let t = tempfile::tempdir().unwrap();
        let r = probe(t.path(), &[]);
        assert_eq!(r.status, GpuIoStatus::None);
        assert!(has(&r, "no /dev/nvidia<N>"));
        assert!(r.note.contains("no I/O was performed"));
    }

    #[test]
    fn control_node_alone_is_not_a_gpu() {
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "proc/modules", "nvidia 1 0 - Live 0x0\n");
        put(t.path(), "dev/nvidiactl", "");
        assert_eq!(probe(t.path(), URING).status, GpuIoStatus::None);
    }

    #[test]
    fn nvidia_fs_proc_dir_alone_never_makes_gds() {
        // Fabricated-looking tree: nvidia-fs files and a GPU node but no loaded modules.
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "proc/driver/nvidia-fs/stats", "x\n");
        put(t.path(), "dev/nvidia0", "");
        let r = probe(t.path(), URING);
        assert_eq!(r.status, GpuIoStatus::None);
    }

    #[test]
    fn malformed_cufile_never_claims_gds() {
        let t = gds_tree();
        put(t.path(), "etc/cufile.json", "{ not json");
        let r = probe(t.path(), URING);
        assert_ne!(r.status, GpuIoStatus::None);
        assert!(has(&r, "not valid JSON"));
        assert_eq!(r.evidence.allow_compat_mode, None);
        // The GDS conditions are still met; the malformed file is reported, not hidden.
        assert!(has(&r, "not that GDS works"));
        put(t.path(), "etc/cufile.json", r#"{"properties":{"force_compat_mode":"yes"}}"#);
        assert!(has(&probe(t.path(), URING), "not a boolean"));
        put(t.path(), "etc/cufile.json", r#"{"other":1}"#);
        assert!(has(&probe(t.path(), URING), "no \"properties\""));
    }

    #[test]
    fn force_compat_mode_blocks_gds() {
        let t = gds_tree();
        put(
            t.path(),
            "etc/cufile.json",
            r#"{"properties":{"force_compat_mode":true,"allow_compat_mode":false}}"#,
        );
        let r = probe(t.path(), URING);
        assert_eq!(r.status, GpuIoStatus::Compat);
        assert!(has(&r, "force_compat_mode=true"));
        assert!(has(&r, "fail rather than fall back"));
    }

    #[test]
    fn unreadable_files_are_reported_and_never_counted_as_a_pass() {
        let t = gds_tree();
        // A directory where a file is expected fails with a non-NotFound error on any user.
        fs::remove_file(t.path().join("proc/sys/kernel/osrelease")).unwrap();
        fs::create_dir_all(t.path().join("proc/sys/kernel/osrelease")).unwrap();
        let r = probe(t.path(), URING);
        assert_eq!(r.status, GpuIoStatus::Compat);
        assert!(has(&r, "could not read /proc/sys/kernel/osrelease"));
        assert!(has(&r, "kernel version could not be determined"));
    }

    #[test]
    fn permission_denied_is_reported() {
        let t = gds_tree();
        let p = t.path().join("proc/driver/nvidia/version");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&p).is_ok() {
            return; // running as root: permissions are not enforced
        }
        let r = probe(t.path(), URING);
        assert_eq!(r.status, GpuIoStatus::Compat);
        assert!(has(&r, "could not read /proc/driver/nvidia/version"));
    }

    #[test]
    fn old_kernel_is_compat() {
        let t = gds_tree();
        put(t.path(), "proc/sys/kernel/osrelease", "5.15.0-100-generic\n");
        let r = probe(t.path(), URING);
        assert_eq!(r.status, GpuIoStatus::Compat);
        assert_eq!(r.evidence.kernel_ok, Some(false));
        assert!(has(&r, "older than 6.2"));
    }

    #[test]
    fn closed_or_old_driver_is_compat() {
        let t = gds_tree();
        put(t.path(), "proc/driver/nvidia/version", "NVRM version: NVIDIA UNIX x86_64 Kernel Module  550.54.14\n");
        let r = probe(t.path(), URING);
        assert_eq!(r.status, GpuIoStatus::Compat);
        assert!(has(&r, "open kernel module"));
    }

    #[test]
    fn non_direct_backends_are_never_gds() {
        let t = gds_tree();
        for b in [&[][..], &[DeviceBackend::File], &[DeviceBackend::Aligned]] {
            let r = probe(t.path(), b);
            assert_eq!(r.status, GpuIoStatus::Compat);
            assert!(has(&r, "no io_uring"));
        }
    }

    #[test]
    fn serializes_with_lowercase_status() {
        let t = tempfile::tempdir().unwrap();
        let v = serde_json::to_value(probe(t.path(), &[DeviceBackend::File])).unwrap();
        assert_eq!(v["status"], "none");
        assert_eq!(v["probe_only"], true);
        assert_eq!(v["verified_on_hardware"], false);
        assert_eq!(v["evidence"]["backends"][0], "file");
    }

    #[test]
    fn comment_stripper_keeps_strings() {
        assert_eq!(
            strip_json_comments("{\"a\":\"//x\"} // t\n/* c */"),
            "{\"a\":\"//x\"} \n"
        );
    }
}
