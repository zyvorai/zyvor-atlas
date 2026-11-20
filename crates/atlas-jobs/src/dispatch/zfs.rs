// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Provision a raw, unformatted local block device into a brand-new ZFS pool (`zpool create`).
//! Local-host-only — see `JobSpec::ZfsPoolCreateFromDevice`'s doc comment.

use std::sync::Arc;

use anyhow::Result;
use atlas_api_types::{
    DiscoveryResult, Health, StorageCluster, StorageHealth, StoragePool, StorageVolume, VolumeKind,
};
use atlas_driver_k8s::K8sDriver;
use sqlx::AnyPool;

use crate::spec::JobSpec;

/// The only device path fake mode will ever "succeed" against — never a real disk, so a fake-mode
/// gateway can never be tricked (by a malformed spec, a bug, or a bad request that slipped past
/// the route layer) into believing it formatted an operator's actual hardware. Deliberately a
/// syntactically valid whole-disk name (passes `validate_raw_device_path` like any real device
/// would) rather than an obviously-fake string, so the fixture exercises the exact same request
/// shape a real one would.
pub(crate) const FAKE_FIXTURE_DEVICE: &str = "/dev/vdz";

/// Fake driver mode has no `zpool`/`lsblk`/`findmnt`/`wipefs` binaries in the image — job handlers
/// must skip shelling out and simulate success only for the fixture device above, mirroring
/// `atlas-driver-ceph`'s `is_fake_ceph_mode()` convention (re-read from env here since job
/// dispatch has no `AppState` handle).
fn is_fake_zfs_mode() -> bool {
    !matches!(
        std::env::var("ATLAS_ZFS_DRIVER_MODE")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "real"
    )
}

pub(crate) async fn dispatch_zfs(
    pool: &AnyPool,
    _k8s: &Option<Arc<K8sDriver>>,
    _tenant_id: &str,
    spec: JobSpec,
) -> Result<serde_json::Value> {
    match spec {
        JobSpec::ZfsPoolDestroy {
            backend_id,
            pool_name,
            confirmed_pool_name,
        } => destroy_pool(pool, &backend_id, &pool_name, &confirmed_pool_name).await,
        spec => create_pool_from_device(pool, spec).await,
    }
}

/// `zpool destroy`, then drop the pool's inventory rows. Every refusal below is checked again here
/// (the route checked too) because the job may run long after it was enqueued.
async fn destroy_pool(
    pool: &AnyPool,
    backend_id: &str,
    pool_name: &str,
    confirmed_pool_name: &str,
) -> Result<serde_json::Value> {
    anyhow::ensure!(
        pool_name == confirmed_pool_name,
        "pool name confirmation mismatch"
    );
    let pool_row_id = atlas_driver_zfs::pool_id(pool_name);
    anyhow::ensure!(
        atlas_inventory::pool_row_exists(pool, &pool_row_id).await?,
        "pool {pool_name} is not in Atlas's inventory — refusing to destroy a pool Atlas did not provision or discover"
    );
    let root_volume = atlas_driver_zfs::root_volume_id(pool_name);
    let extra_volumes: Vec<String> = atlas_inventory::volume_ids_in_pool(pool, &pool_row_id)
        .await?
        .into_iter()
        .filter(|v| *v != root_volume)
        .collect();
    anyhow::ensure!(
        extra_volumes.is_empty(),
        "refusing to destroy {pool_name}: inventory still lists {} volume(s) in it ({})",
        extra_volumes.len(),
        extra_volumes.join(", ")
    );

    let mut destroyed = false;
    if !is_fake_zfs_mode() {
        let present = atlas_driver_zfs::list_zpool_names().await?;
        if present.iter().any(|p| p == pool_name) {
            let datasets = atlas_driver_zfs::list_pool_datasets(pool_name).await?;
            let extra = atlas_driver_zfs::non_root_datasets(pool_name, &datasets);
            anyhow::ensure!(
                extra.is_empty(),
                "refusing to destroy {pool_name}: it still holds {} dataset(s) ({})",
                extra.len(),
                extra.iter().map(|d| d.as_str()).collect::<Vec<_>>().join(", ")
            );
            atlas_driver_zfs::zpool_destroy(pool_name).await?;
            destroyed = true;
        }
        // Absent from a successful `zpool list`: the pool is already gone on the host (destroyed
        // or exported out of band), so only the stale inventory rows remain to be dropped.
    } else {
        destroyed = true;
    }
    atlas_inventory::delete_pool_with_volumes(pool, &pool_row_id).await?;
    Ok(serde_json::json!({
        "pool": pool_name,
        "backend_id": backend_id,
        "destroyed_on_host": destroyed,
    }))
}

async fn create_pool_from_device(pool: &AnyPool, spec: JobSpec) -> Result<serde_json::Value> {
    let JobSpec::ZfsPoolCreateFromDevice {
        backend_id,
        pool_name,
        device_path,
        confirmed_device_path,
        host,
        wipe_existing,
    } = spec
    else {
        anyhow::bail!("not a zfs spec");
    };
    anyhow::ensure!(
        device_path == confirmed_device_path,
        "device path confirmation mismatch"
    );
    atlas_common::device::validate_raw_device_path(&device_path).map_err(anyhow::Error::msg)?;

    if is_fake_zfs_mode() {
        anyhow::ensure!(
            device_path == FAKE_FIXTURE_DEVICE,
            "fake mode only accepts the fixture device {FAKE_FIXTURE_DEVICE}"
        );
    } else {
        let check = atlas_driver_zfs::inspect_device(&device_path).await?;
        if let Some(reason) = check.hard_refusal_reason() {
            anyhow::bail!("refusing to format {device_path}: {reason}");
        }
        if wipe_existing {
            atlas_driver_zfs::wipe_device(&device_path).await?;
            // wipefs -a itself is synchronous, but re-probing the device right after can still
            // see a stale udev-cached pttype/fstype for a short window — the same
            // udev-database-propagation lag responsible for zpool_create's own reopen race (see
            // its doc comment). Confirmed live: a recheck here reported a leftover "gpt" pttype
            // immediately after a wipe that a plain read-only re-probe moments later showed was
            // already fully clean. Retry the recheck itself before giving up.
            const MAX_RECHECK_ATTEMPTS: u32 = 5;
            let mut last_reason = None;
            for attempt in 1..=MAX_RECHECK_ATTEMPTS {
                let recheck = atlas_driver_zfs::inspect_device(&device_path).await?;
                match recheck.refusal_reason_after_wipe() {
                    None => {
                        last_reason = None;
                        break;
                    }
                    Some(reason) => {
                        last_reason = Some(reason);
                        if attempt < MAX_RECHECK_ATTEMPTS {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                    }
                }
            }
            if let Some(reason) = last_reason {
                anyhow::bail!("refusing to format {device_path} even after wipe: {reason}");
            }
        } else if let Some(reason) = check.refusal_reason() {
            anyhow::bail!("refusing to format {device_path}: {reason}");
        }
        atlas_driver_zfs::zpool_create(&pool_name, &device_path).await?;
    }

    // Synthesize the inventory rows directly instead of relying on a follow-up discovery pass:
    // `RealZfsDriver.zpools` is a fixed Vec baked in at gateway startup from `ATLAS_ZFS_POOLS`, so
    // a subsequent discovery pass alone would never pick up a pool that wasn't in that list when
    // the process started. This reuses `upsert_discovery`'s existing insert/ON-CONFLICT logic
    // wholesale (`authoritative=false`: never prunes anything else) instead of hand-writing new
    // SQL, and uses the exact id scheme `RealZfsDriver`/`FakeZfsDriver` already derive, so a later
    // real discovery pass (once the pool name is added to `ATLAS_ZFS_POOLS` and the gateway is
    // restarted) upserts onto this same row rather than creating a duplicate. Known limitation of
    // this first slice: capacity/health on this row go stale until that restart happens.
    let cluster_id = atlas_driver_zfs::cluster_id(&host);
    let pool_row_id = atlas_driver_zfs::pool_id(&pool_name);
    let discovery = DiscoveryResult {
        cluster: StorageCluster {
            id: cluster_id.clone(),
            backend_id: backend_id.clone(),
            name: format!("zfs://{host}"),
            native_fsid: None,
            health: Health::Ok,
            raw_capacity_bytes: None,
            used_capacity_bytes: None,
            available_capacity_bytes: None,
        },
        pools: vec![StoragePool {
            id: pool_row_id.clone(),
            cluster_id: cluster_id.clone(),
            name: pool_name.clone(),
            kind: "zpool".into(),
            device_class: None,
            replica_size: None,
            used_bytes: None,
            max_bytes: None,
            health: Health::Ok,
        }],
        osds: vec![],
        volumes: vec![StorageVolume {
            id: atlas_driver_zfs::root_volume_id(&pool_name),
            cluster_id: Some(cluster_id.clone()),
            pool_id: Some(pool_row_id.clone()),
            name: pool_name.clone(),
            kind: VolumeKind::Filesystem,
            backend_native_id: Some(format!("{host}:{pool_name}")),
            size_bytes: 0,
            used_bytes: Some(0),
            state: "available".into(),
            health: Health::Ok,
            kubernetes_namespace: None,
            pvc_name: None,
            storage_class_name: None,
        }],
        health: StorageHealth {
            status: Health::Ok,
            summary: format!("zpool {pool_name} created from {device_path} on {host}"),
            raw_capacity_bytes: None,
            used_capacity_bytes: None,
            available_capacity_bytes: None,
            recovering: false,
            degraded_objects: 0,
        },
    };
    atlas_inventory::upsert_discovery(pool, &backend_id, &discovery, false).await?;

    Ok(serde_json::json!({
        "pool": pool_name,
        "device": device_path,
        "host": host,
        "backend_id": backend_id,
    }))
}
