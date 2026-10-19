// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
mod databridge;
mod helpers;
mod object;
mod rbd;
mod rook;
mod selftest;
mod volumes;
mod macos;
mod zfs;

use std::sync::Arc;

use anyhow::Result;
use atlas_driver_k8s::K8sDriver;
use sqlx::AnyPool;

use crate::spec::JobSpec;

#[tracing::instrument(skip(pool, k8s, spec), fields(tenant_id = %tenant_id))]
pub(crate) async fn dispatch(
    pool: &AnyPool,
    k8s: &Option<Arc<K8sDriver>>,
    tenant_id: &str,
    spec: JobSpec,
) -> Result<serde_json::Value> {
    match &spec {
        JobSpec::RbdCreate { .. }
        | JobSpec::RbdDelete { .. }
        | JobSpec::RbdClone { .. }
        | JobSpec::RbdResize { .. }
        | JobSpec::RbdMigrate { .. }
        | JobSpec::RbdFlatten { .. }
        | JobSpec::RbdQos { .. }
        | JobSpec::CephOsdOp { .. }
        | JobSpec::RbdMirror { .. }
        | JobSpec::RbdSnapshot { .. }
        | JobSpec::RbdRollback { .. }
        | JobSpec::RbdGroupRollback { .. }
        | JobSpec::RbdSnapDelete { .. } => rbd::dispatch_rbd(pool, k8s, tenant_id, spec).await,
        JobSpec::VolumeCreate { .. }
        | JobSpec::VolumeDelete { .. }
        | JobSpec::VolumeExpand { .. }
        | JobSpec::SnapshotCreate { .. }
        | JobSpec::SnapshotDelete { .. }
        | JobSpec::SnapshotClone { .. } => {
            volumes::dispatch_volumes(pool, k8s, tenant_id, spec).await
        }
        JobSpec::BucketCreate { .. }
        | JobSpec::BackupCreate { .. }
        | JobSpec::RestoreBackup { .. }
        | JobSpec::BackupDelete { .. }
        | JobSpec::BucketDelete { .. } => {
            object::dispatch_object(pool, k8s, tenant_id, spec).await
        }
        JobSpec::BucketCreateRustfs { .. }
        | JobSpec::BucketDeleteRustfs { .. }
        | JobSpec::RustfsDriveProvision { .. }
        | JobSpec::RustfsInstance { .. } => {
            anyhow::bail!(
                "RustFS job variants are retired; reprovision on Ceph RGW (bkd_ceph_lab) or a generic S3 endpoint"
            )
        }
        JobSpec::CephPoolCreate { .. }
        | JobSpec::CephPoolDelete { .. }
        | JobSpec::CephFilesystemCreate { .. }
        | JobSpec::CephFilesystemDelete { .. }
        | JobSpec::CephObjectStoreCreate { .. }
        | JobSpec::CephObjectStoreDelete { .. }
        | JobSpec::CephOsdAddDevice { .. } => rook::dispatch_rook(pool, k8s, tenant_id, spec).await,
        JobSpec::ZfsPoolCreateFromDevice { .. } | JobSpec::ZfsPoolDestroy { .. } => {
            zfs::dispatch_zfs(pool, k8s, tenant_id, spec).await
        }
        JobSpec::MacosDiskErase { .. }
        | JobSpec::MacosVolumeAdd { .. }
        | JobSpec::MacosVolumeDelete { .. }
        | JobSpec::MacosContainerResize { .. }
        | JobSpec::MacosDiskEject { .. } => macos::dispatch_macos(pool, spec).await,
        JobSpec::S3BackendSelfTest { .. } => selftest::dispatch_selftest(pool, k8s, spec).await,
        JobSpec::SourceDiscover { .. }
        | JobSpec::MigrationAssess { .. }
        | JobSpec::EdgeDbProvision { .. }
        | JobSpec::FullLoad { .. }
        | JobSpec::CdcStart { .. }
        | JobSpec::CdcStop { .. }
        | JobSpec::CdcRestart { .. }
        | JobSpec::ValidateRun { .. }
        | JobSpec::Cutover { .. }
        | JobSpec::Rollback { .. }
        | JobSpec::ObjectMigrate { .. } => databridge::dispatch_databridge(pool, k8s, spec).await,
    }
}
