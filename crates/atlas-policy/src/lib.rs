// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Intent → placement resolution (PDF §12.3 default policies).
//!
//! Products request *intent* ("production", "database", ...) and Atlas decides the concrete
//! backend placement (StorageClass, access mode, volume mode). This keeps Ceph internals out of
//! every product. The MVP ships a built-in policy table; later slices load policies from the
//! `storage_policies` table / CRDs.

use atlas_api_types::{Placement, VolumeKind};

/// A built-in policy definition.
#[derive(Debug, Clone)]
pub struct Policy {
    pub intent: &'static str,
    pub storage_class: &'static str,
    pub access_mode: &'static str,
    pub volume_mode: &'static str,
    /// The volume kind this policy actually provisions — authoritative over whatever `kind` the
    /// caller passed in, since a named policy fully determines the backing storage (e.g. `shared`
    /// is always CephFS/`Filesystem`, never `Block`, no matter what the request said or defaulted to).
    pub kind: VolumeKind,
    pub description: &'static str,
}

/// Default block/file StorageClasses (match the Rook lab manifests, PDF §7.1).
pub const DEFAULT_BLOCK_SC: &str = "zyvor-rbd-prod";
pub const DEFAULT_FILE_SC: &str = "zyvor-cephfs-shared";

/// The built-in policy catalog (PDF §12.3).
pub const POLICIES: &[Policy] = &[
    Policy {
        intent: "production",
        storage_class: DEFAULT_BLOCK_SC,
        access_mode: "ReadWriteOnce",
        volume_mode: "Filesystem",
        kind: VolumeKind::Block,
        description: "Business VMs / app servers — RBD, 3 replicas, daily snapshots",
    },
    Policy {
        intent: "database",
        storage_class: DEFAULT_BLOCK_SC,
        access_mode: "ReadWriteOnce",
        volume_mode: "Filesystem",
        kind: VolumeKind::Block,
        description: "Databases — RBD NVMe, hourly snapshots, daily backup",
    },
    Policy {
        intent: "development",
        storage_class: DEFAULT_BLOCK_SC,
        access_mode: "ReadWriteOnce",
        volume_mode: "Filesystem",
        kind: VolumeKind::Block,
        description: "Dev/test VMs — RBD, 2 replicas, manual snapshots",
    },
    Policy {
        intent: "shared",
        storage_class: DEFAULT_FILE_SC,
        access_mode: "ReadWriteMany",
        volume_mode: "Filesystem",
        kind: VolumeKind::Filesystem,
        description: "ISO library, templates, shared reports — CephFS RWX",
    },
    Policy {
        intent: "ai",
        storage_class: DEFAULT_BLOCK_SC,
        access_mode: "ReadWriteOnce",
        volume_mode: "Filesystem",
        kind: VolumeKind::Block,
        description: "AI datasets / model workspaces — high-throughput RBD/CephFS",
    },
];

/// Look up a policy by intent name.
pub fn find(intent: &str) -> Option<&'static Policy> {
    POLICIES.iter().find(|p| p.intent == intent)
}

/// Default backend for new object (bucket) provisioning — Ceph RGW. Deliberately not
/// part of the [`Policy`]/[`resolve`] machinery above: those describe StorageClass/access-mode/
/// volume-mode placement for PVC-shaped provisioning, none of which apply to an S3 bucket. This is
/// the one thing bucket creation actually needs resolved with an explicit-override-wins default,
/// so the choice lives in one named, testable place instead of a route-handler literal.
pub const DEFAULT_OBJECT_BACKEND: &str = "bkd_ceph_lab";

/// Resolve which backend provisions a new bucket: an explicit `backend_id` always wins; otherwise
/// [`DEFAULT_OBJECT_BACKEND`].
pub fn resolve_object_backend(explicit: Option<&str>) -> String {
    explicit
        .map(str::to_string)
        .unwrap_or_else(|| DEFAULT_OBJECT_BACKEND.to_string())
}

/// Resolve a placement decision from an optional intent and the requested volume kind.
///
/// Precedence: explicit `storage_class_override` › named policy › kind default.
///
/// A `Some(intent)` that doesn't match any entry in [`POLICIES`] is an error (most likely a
/// typo) rather than a silent fall-through to the kind default — the two cases ("no policy
/// given" vs. "policy given but unrecognized") were previously indistinguishable to the caller,
/// so a typo'd `policy` field would provision storage silently rather than failing loudly.
pub fn resolve(
    intent: Option<&str>,
    kind: VolumeKind,
    storage_class_override: Option<&str>,
) -> Result<Placement, String> {
    // Explicit override wins — the caller is taking full control of placement, so its `kind`
    // (not a policy's) is authoritative here.
    if let Some(sc) = storage_class_override {
        let (access, mode) = defaults_for_kind(kind);
        return Ok(Placement {
            intent: intent.unwrap_or("custom").to_string(),
            storage_class: sc.to_string(),
            access_mode: access.to_string(),
            volume_mode: mode.to_string(),
            kind,
        });
    }
    // Named policy — the policy fully determines the backing storage, so its `kind` overrides
    // whatever the caller passed (which may just be the client library's default).
    if let Some(name) = intent {
        return match find(name) {
            Some(p) => Ok(Placement {
                intent: p.intent.to_string(),
                storage_class: p.storage_class.to_string(),
                access_mode: p.access_mode.to_string(),
                volume_mode: p.volume_mode.to_string(),
                kind: p.kind,
            }),
            None => Err(format!(
                "unknown policy {name:?} — known policies: {}",
                POLICIES
                    .iter()
                    .map(|p| p.intent)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        };
    }
    // No intent given at all — fall back to a sensible default for the kind.
    let (access, mode) = defaults_for_kind(kind);
    Ok(Placement {
        intent: "default".to_string(),
        storage_class: match kind {
            VolumeKind::Filesystem => DEFAULT_FILE_SC.to_string(),
            _ => DEFAULT_BLOCK_SC.to_string(),
        },
        access_mode: access.to_string(),
        volume_mode: mode.to_string(),
        kind,
    })
}

fn defaults_for_kind(kind: VolumeKind) -> (&'static str, &'static str) {
    match kind {
        VolumeKind::Filesystem => ("ReadWriteMany", "Filesystem"),
        VolumeKind::Block => ("ReadWriteOnce", "Filesystem"),
        VolumeKind::Object => ("ReadWriteOnce", "Filesystem"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_policy_resolves() {
        let p = resolve(Some("database"), VolumeKind::Block, None).unwrap();
        assert_eq!(p.storage_class, DEFAULT_BLOCK_SC);
        assert_eq!(p.access_mode, "ReadWriteOnce");
        assert_eq!(p.intent, "database");
    }

    #[test]
    fn shared_uses_cephfs_rwx() {
        let p = resolve(Some("shared"), VolumeKind::Filesystem, None).unwrap();
        assert_eq!(p.storage_class, DEFAULT_FILE_SC);
        assert_eq!(p.access_mode, "ReadWriteMany");
    }

    #[test]
    fn override_wins() {
        let p = resolve(Some("production"), VolumeKind::Block, Some("custom-sc")).unwrap();
        assert_eq!(p.storage_class, "custom-sc");
    }

    #[test]
    fn no_intent_falls_back_by_kind() {
        let p = resolve(None, VolumeKind::Filesystem, None).unwrap();
        assert_eq!(p.storage_class, DEFAULT_FILE_SC);
    }

    #[test]
    fn unknown_intent_errors() {
        let err = resolve(Some("nonsense"), VolumeKind::Filesystem, None).unwrap_err();
        assert!(err.contains("unknown policy"));
    }

    #[test]
    fn override_ignores_unknown_intent() {
        // storage_class_override wins outright — an unrecognized intent alongside it is not an
        // error since it's purely a cosmetic label at that point (PDF §12.3 override precedence).
        let p = resolve(Some("nonsense"), VolumeKind::Block, Some("custom-sc")).unwrap();
        assert_eq!(p.storage_class, "custom-sc");
    }

    #[test]
    fn resolve_object_backend_explicit_override_wins() {
        assert_eq!(resolve_object_backend(Some("bkd_ceph_lab")), "bkd_ceph_lab");
    }

    #[test]
    fn resolve_object_backend_defaults_to_ceph_rgw() {
        assert_eq!(resolve_object_backend(None), DEFAULT_OBJECT_BACKEND);
        assert_eq!(DEFAULT_OBJECT_BACKEND, "bkd_ceph_lab");
    }
}
