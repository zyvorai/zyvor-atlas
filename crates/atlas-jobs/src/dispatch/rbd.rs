// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
use anyhow::{Context, Result};
use atlas_api_types::{Health, StorageVolume, VolumeKind};
use atlas_driver_k8s::K8sDriver;
use sqlx::AnyPool;
use std::sync::Arc;

use crate::spec::JobSpec;

/// Fake driver mode has no `rbd`/`ceph` CLI in the image — job handlers must skip shelling out
/// and simulate success against the inventory catalog instead (mirrors `AppState::config`'s
/// `CephDriverMode`, re-read from env here since job dispatch has no `AppState` handle).
fn is_fake_ceph_mode() -> bool {
    !matches!(
        std::env::var("ATLAS_CEPH_DRIVER_MODE")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "real"
    )
}

pub(crate) async fn dispatch_rbd(
    pool: &AnyPool,
    _k8s: &Option<Arc<K8sDriver>>,
    tenant_id: &str,
    spec: JobSpec,
) -> Result<serde_json::Value> {
    match spec {
        JobSpec::RbdCreate {
            volume_id,
            backend_id,
            pool: rbd_pool,
            image,
            size_bytes,
        } => {
            // Idempotent retry: if a prior attempt already created the image (e.g. dispatch
            // succeeded but the job's mark-succeeded write failed, forcing a full re-run), `rbd
            // create` errors on an image that already exists — skip it and just re-record.
            if !is_fake_ceph_mode()
                && atlas_driver_ceph::rbd_info_size(&rbd_pool, &image)
                    .await
                    .is_err()
            {
                atlas_driver_ceph::rbd_create(&rbd_pool, &image, size_bytes)
                    .await
                    .with_context(|| format!("rbd create {rbd_pool}/{image}"))?;
            }
            let vol = StorageVolume {
                id: volume_id.clone(),
                cluster_id: None,
                pool_id: None,
                name: image.clone(),
                kind: VolumeKind::Block,
                backend_native_id: Some(format!("rbd:{rbd_pool}/{image}")),
                size_bytes,
                used_bytes: None,
                state: "available".into(),
                health: Health::Ok,
                kubernetes_namespace: None,
                pvc_name: None,
                storage_class_name: None,
            };
            atlas_inventory::upsert_volume(pool, &backend_id, tenant_id, &vol, None).await?;
            Ok(serde_json::json!({
                "volume_id": volume_id, "rbd": format!("{rbd_pool}/{image}"),
                "size_bytes": size_bytes, "state": "available"
            }))
        }
        JobSpec::RbdDelete {
            volume_id,
            pool: rbd_pool,
            image,
        } => {
            if is_fake_ceph_mode() {
                atlas_inventory::rbd_snapshots::delete_all_for_image(pool, &rbd_pool, &image)
                    .await?;
            } else {
                atlas_driver_ceph::rbd_remove(&rbd_pool, &image)
                    .await
                    .with_context(|| format!("rbd rm {rbd_pool}/{image}"))?;
            }
            atlas_inventory::delete_volume_row(pool, &volume_id).await?;
            Ok(serde_json::json!({
                "volume_id": volume_id, "rbd": format!("{rbd_pool}/{image}"), "deleted": true
            }))
        }
        JobSpec::RbdClone {
            volume_id,
            backend_id,
            pool: rbd_pool,
            image,
            snap,
            clone_image,
        } => {
            // Idempotent retry: if the clone image already exists, a prior attempt already
            // finished (only the job's own bookkeeping failed afterwards) — re-running `rbd snap
            // create`/`rbd snap protect` on an already-existing/protected snapshot errors, so skip
            // straight to recording it.
            let size_bytes = if is_fake_ceph_mode() {
                // No `rbd` CLI to ask — a clone starts at the parent's virtual size, so read it
                // back from the catalog we already have. Still record the base snapshot the clone
                // hangs off of, so the parent's Snaps panel matches what real mode would show.
                atlas_inventory::rbd_snapshots::create(pool, &rbd_pool, &image, &snap).await?;
                let parent_native = format!("rbd:{rbd_pool}/{image}");
                atlas_inventory::list_volumes(pool)
                    .await?
                    .into_iter()
                    .find(|v| v.backend_native_id.as_deref() == Some(parent_native.as_str()))
                    .map(|v| v.size_bytes)
                    .unwrap_or(0)
            } else {
                let already_cloned =
                    atlas_driver_ceph::rbd_info_size(&rbd_pool, &clone_image).await;
                if let Ok(size_bytes) = already_cloned {
                    size_bytes
                } else {
                    // Snapshot the source (unless a prior attempt already created it), protect it
                    // (required for cloning), then create the COW clone.
                    let existing_snaps = atlas_driver_ceph::rbd_snap_list(&rbd_pool, &image)
                        .await
                        .unwrap_or_default();
                    if !existing_snaps.iter().any(|s| s == &snap) {
                        atlas_driver_ceph::rbd_snap_create(&rbd_pool, &image, &snap)
                            .await
                            .with_context(|| {
                                format!("rbd snap create {rbd_pool}/{image}@{snap}")
                            })?;
                    }
                    atlas_driver_ceph::rbd_snap_protect(&rbd_pool, &image, &snap)
                        .await
                        .with_context(|| format!("rbd snap protect {rbd_pool}/{image}@{snap}"))?;
                    atlas_driver_ceph::rbd_clone(&rbd_pool, &image, &snap, &rbd_pool, &clone_image)
                        .await
                        .with_context(|| {
                            format!("rbd clone {rbd_pool}/{image}@{snap} -> {clone_image}")
                        })?;
                    atlas_driver_ceph::rbd_info_size(&rbd_pool, &clone_image)
                        .await
                        .unwrap_or(0)
                }
            };
            let vol = StorageVolume {
                id: volume_id.clone(),
                cluster_id: None,
                pool_id: None,
                name: clone_image.clone(),
                kind: VolumeKind::Block,
                backend_native_id: Some(format!("rbd:{rbd_pool}/{clone_image}")),
                size_bytes,
                used_bytes: None,
                state: "available".into(),
                health: Health::Ok,
                kubernetes_namespace: None,
                pvc_name: None,
                storage_class_name: None,
            };
            atlas_inventory::upsert_volume(pool, &backend_id, tenant_id, &vol, None).await?;
            Ok(serde_json::json!({
                "volume_id": volume_id, "clone": format!("{rbd_pool}/{clone_image}"),
                "parent": format!("{rbd_pool}/{image}@{snap}"), "size_bytes": size_bytes
            }))
        }
        JobSpec::RbdResize {
            volume_id,
            pool: rbd_pool,
            image,
            new_size_bytes,
            allow_shrink,
        } => {
            if !is_fake_ceph_mode() {
                atlas_driver_ceph::rbd_resize(&rbd_pool, &image, new_size_bytes, allow_shrink)
                    .await
                    .with_context(|| format!("rbd resize {rbd_pool}/{image}"))?;
            }
            atlas_inventory::set_volume_size(pool, &volume_id, new_size_bytes).await?;
            Ok(serde_json::json!({
                "volume_id": volume_id, "rbd": format!("{rbd_pool}/{image}"),
                "new_size_bytes": new_size_bytes
            }))
        }
        JobSpec::RbdMigrate {
            volume_id,
            pool: rbd_pool,
            image,
            dest_pool,
        } => {
            // Idempotent retry: once `rbd migration commit` lands, the image no longer resolves
            // under the source pool, so a re-run of `rbd migrate` would fail `prepare` against a
            // vanished source — detect an already-completed migration and skip straight through.
            if !is_fake_ceph_mode() {
                let already_migrated = atlas_driver_ceph::rbd_info_size(&dest_pool, &image)
                    .await
                    .is_ok()
                    && atlas_driver_ceph::rbd_info_size(&rbd_pool, &image)
                        .await
                        .is_err();
                if !already_migrated {
                    atlas_driver_ceph::rbd_migrate(&rbd_pool, &image, &dest_pool)
                        .await
                        .with_context(|| {
                            format!("rbd migrate {rbd_pool}/{image} -> {dest_pool}")
                        })?;
                }
            }
            // Record the new pool/image location — otherwise the catalog keeps pointing at the
            // now-defunct source pool. The row's id is derived deterministically from pool+name
            // (matching what create/discovery use — see routes/rbd.rs's create_rbd_image), so a
            // pool move makes the *old* id stale too: rename it, or the next discovery pass
            // derives a different id for the new pool, treats the old row as an orphan, and
            // prunes it — the same volume_id-goes-404 bug the create path had.
            let new_volume_id = format!("vol_{dest_pool}_{image}");
            if !volume_id.is_empty() {
                atlas_inventory::rename_volume_id(
                    pool,
                    &volume_id,
                    &new_volume_id,
                    &format!("rbd:{dest_pool}/{image}"),
                )
                .await?;
            }
            Ok(serde_json::json!({
                "volume_id": new_volume_id, "from": format!("{rbd_pool}/{image}"),
                "to": format!("{dest_pool}/{image}")
            }))
        }
        JobSpec::RbdFlatten {
            pool: rbd_pool,
            image,
        } => {
            // Idempotent retry: `rbd flatten` errors on an image with no parent, which is exactly
            // the state a prior successful attempt leaves behind.
            let spec = format!("{rbd_pool}/{image}");
            if !is_fake_ceph_mode() {
                let has_parent = atlas_driver_ceph::rbd_cmd(&["info", &spec])
                    .await
                    .map(|info| info.get("parent").is_some())
                    .unwrap_or(true);
                if has_parent {
                    atlas_driver_ceph::rbd_flatten(&rbd_pool, &image)
                        .await
                        .with_context(|| format!("rbd flatten {spec}"))?;
                }
            }
            Ok(serde_json::json!({ "rbd": spec, "flattened": true }))
        }
        JobSpec::RbdQos {
            volume_id,
            pool: rbd_pool,
            image,
            iops_limit,
            bps_limit,
        } => {
            if !is_fake_ceph_mode() {
                atlas_driver_ceph::rbd_qos_set(&rbd_pool, &image, iops_limit, bps_limit)
                    .await
                    .with_context(|| format!("rbd qos {rbd_pool}/{image}"))?;
            }
            if !volume_id.is_empty() {
                atlas_inventory::set_volume_qos(pool, &volume_id, iops_limit, bps_limit).await?;
            }
            Ok(serde_json::json!({
                "rbd": format!("{rbd_pool}/{image}"),
                "iops_limit": iops_limit, "bps_limit": bps_limit
            }))
        }
        JobSpec::CephOsdOp {
            osd_id,
            action,
            weight,
        } => {
            if !is_fake_ceph_mode() {
                atlas_driver_ceph::ceph_osd_op(&action, osd_id, weight)
                    .await
                    .with_context(|| format!("ceph osd {action} {osd_id}"))?;
            }
            Ok(
                serde_json::json!({ "osd_id": osd_id, "action": action, "weight": weight, "applied": true }),
            )
        }
        JobSpec::RbdMirror {
            mirror_id,
            pool: rbd_pool,
            image,
            action,
            mode,
            force,
        } => {
            // Fake driver: catalog transitions are applied by the API before enqueue; skip the live
            // `rbd` CLI and do not rewrite role/state here (avoids racing a later demote/promote).
            let fake = is_fake_ceph_mode();
            if fake {
                tracing::info!(
                    %action,
                    force,
                    "fake mode: skipping rbd mirror {rbd_pool}/{image}"
                );
            } else if let Err(e) =
                atlas_driver_ceph::rbd_mirror_op(&action, &rbd_pool, &image, &mode, force).await
            {
                let err = format!("{e:#}");
                let _ = atlas_inventory::dr::set_mirror_error(pool, &mirror_id, &err).await;
                return Err(e).with_context(|| format!("rbd mirror {action} {rbd_pool}/{image}"));
            } else {
                // Real mode: reflect the resulting role/state after the CLI succeeds.
                // `enable` only clears pending state — role was set by the API at insert time.
                match action.as_str() {
                    "promote" => {
                        atlas_inventory::dr::set_mirror(pool, &mirror_id, "primary", "enabled")
                            .await?;
                        let _ = atlas_inventory::dr::record_failover(pool, &mirror_id, force).await;
                    }
                    "demote" | "resync" => {
                        atlas_inventory::dr::set_mirror(pool, &mirror_id, "secondary", "enabled")
                            .await?;
                    }
                    "disable" | "leave-pool" => {
                        atlas_inventory::dr::set_mirror(pool, &mirror_id, "primary", "disabled")
                            .await?;
                    }
                    "enable" | "join-pool" => {
                        atlas_inventory::dr::set_mirror_state(pool, &mirror_id, "enabled").await?;
                    }
                    _ => {}
                }
            }
            Ok(serde_json::json!({
                "mirror_id": mirror_id, "rbd": format!("{rbd_pool}/{image}"),
                "action": action, "force": force, "fake": fake
            }))
        }
        JobSpec::RbdSnapshot {
            pool: rbd_pool,
            image,
            snap,
        } => {
            if is_fake_ceph_mode() {
                atlas_inventory::rbd_snapshots::create(pool, &rbd_pool, &image, &snap).await?;
            } else {
                atlas_driver_ceph::rbd_snap_create(&rbd_pool, &image, &snap)
                    .await
                    .with_context(|| format!("rbd snap create {rbd_pool}/{image}@{snap}"))?;
            }
            Ok(serde_json::json!({ "snapshot": format!("{rbd_pool}/{image}@{snap}") }))
        }
        JobSpec::RbdGroupRollback {
            pool: rbd_pool,
            group,
            snap,
        } => {
            if !is_fake_ceph_mode() {
                atlas_driver_ceph::rbd_group_snap_rollback(&rbd_pool, &group, &snap)
                    .await
                    .with_context(|| format!("rbd group snap rollback {rbd_pool}/{group}@{snap}"))?;
            }
            Ok(serde_json::json!({
                "group": format!("{rbd_pool}/{group}"), "rolled_back_to": snap
            }))
        }
        JobSpec::RbdRollback {
            pool: rbd_pool,
            image,
            snap,
        } => {
            if !is_fake_ceph_mode() {
                atlas_driver_ceph::rbd_snap_rollback(&rbd_pool, &image, &snap)
                    .await
                    .with_context(|| format!("rbd snap rollback {rbd_pool}/{image}@{snap}"))?;
            }
            Ok(serde_json::json!({
                "rbd": format!("{rbd_pool}/{image}"), "rolled_back_to": snap
            }))
        }
        JobSpec::RbdSnapDelete {
            pool: rbd_pool,
            image,
            snap,
        } => {
            // Unprotect is a no-op if the snapshot was never protected (e.g. it was created
            // manually, not via Clone) — always attempt it before rm rather than requiring the
            // caller to know whether a clone was ever taken from it.
            if is_fake_ceph_mode() {
                atlas_inventory::rbd_snapshots::delete(pool, &rbd_pool, &image, &snap).await?;
            } else {
                atlas_driver_ceph::rbd_snap_unprotect(&rbd_pool, &image, &snap)
                    .await
                    .with_context(|| format!("rbd snap unprotect {rbd_pool}/{image}@{snap}"))?;
                atlas_driver_ceph::rbd_snap_rm(&rbd_pool, &image, &snap)
                    .await
                    .with_context(|| format!("rbd snap rm {rbd_pool}/{image}@{snap}"))?;
            }
            Ok(serde_json::json!({ "deleted_snapshot": format!("{rbd_pool}/{image}@{snap}") }))
        }
        _ => anyhow::bail!("not an rbd spec"),
    }
}
