// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Extra I/O probes compiled into `bpf/atlas_bio.bpf.c` and attached best-effort by the live
//! source. A missing tracepoint (no NFS client, no OpenZFS probes, an older io_uring) leaves that
//! family in `programs_missing`; block-layer attach does not depend on them.

/// One optional tracepoint program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeSpec {
    /// Health-report family (`atlas_nfs`, `atlas_zfs`, `atlas_uring`).
    pub family: &'static str,
    /// ELF function name (`aya` program name).
    pub program: &'static str,
    /// Tracepoint category (`attach`'s first argument).
    pub category: &'static str,
    /// Tracepoint name within the category.
    pub tracepoint: &'static str,
}

/// NFS client, OpenZFS and io_uring probes. The cgroup→volume map is a BPF map (`atlas_cgroup`),
/// not a program, so it is not listed here.
pub const EXTRA_PROBES: &[ProbeSpec] = &[
    ProbeSpec {
        family: "atlas_nfs",
        program: "atlas_nfs_read",
        category: "nfs",
        tracepoint: "nfs_initiate_read",
    },
    ProbeSpec {
        family: "atlas_nfs",
        program: "atlas_nfs_write",
        category: "nfs",
        tracepoint: "nfs_initiate_write",
    },
    ProbeSpec {
        family: "atlas_zfs",
        program: "atlas_zfs_read",
        category: "zfs",
        tracepoint: "zfs_read",
    },
    ProbeSpec {
        family: "atlas_zfs",
        program: "atlas_zfs_write",
        category: "zfs",
        tracepoint: "zfs_write",
    },
    ProbeSpec {
        family: "atlas_uring",
        program: "atlas_uring_submit",
        category: "io_uring",
        tracepoint: "io_uring_submit_req",
    },
    ProbeSpec {
        family: "atlas_uring",
        program: "atlas_uring_complete",
        category: "io_uring",
        tracepoint: "io_uring_complete",
    },
];

/// Families the sensor knows about besides the block layer and the native maps.
pub const OPTIONAL_FAMILIES: [&str; 4] = ["atlas_cgroup", "atlas_nfs", "atlas_zfs", "atlas_uring"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_covers_nfs_zfs_and_uring() {
        for family in ["atlas_nfs", "atlas_zfs", "atlas_uring"] {
            let n = EXTRA_PROBES.iter().filter(|p| p.family == family).count();
            assert_eq!(n, 2, "{family}");
        }
        assert!(OPTIONAL_FAMILIES.contains(&"atlas_cgroup"));
        assert!(EXTRA_PROBES
            .iter()
            .any(|p| p.program == "atlas_nfs_read" && p.category == "nfs"));
        assert!(EXTRA_PROBES
            .iter()
            .any(|p| p.tracepoint == "io_uring_complete"));
    }
}
