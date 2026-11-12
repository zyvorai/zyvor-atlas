// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
use anyhow::{anyhow, Context, Result};
use atlas_api_types::{Health, StorageVolume, VolumeKind};
use atlas_driver_k8s::{K8sDriver, PvcCreateSpec};
use sqlx::AnyPool;
use std::sync::Arc;

use super::helpers::{
    build_s3_target, poll_configmap, poll_pvc_phase, poll_snapshot_ready,
    provision_from_snapshot, read_backup_manifest, require_k8s, sha256_hex,
};
use crate::spec::JobSpec;

/// Destination S3 endpoint for generic object jobs (self-test, BYO S3).
/// Prefers `ATLAS_S3_ENDPOINT`, then the public RGW override.
pub(crate) fn s3_endpoint() -> String {
    std::env::var("ATLAS_S3_ENDPOINT")
        .or_else(|_| std::env::var("ATLAS_RGW_PUBLIC_ENDPOINT"))
        .unwrap_or_else(|_| "http://127.0.0.1:7480".into())
}

pub(crate) async fn dispatch_object(
    pool: &AnyPool,
    k8s: &Option<Arc<K8sDriver>>,
    tenant_id: &str,
    spec: JobSpec,
) -> Result<serde_json::Value> {
    match spec {
        JobSpec::RestoreBackup {
            backup_id,
            new_volume_id,
            backend_id,
            snapshot_id,
            snapshot_k8s_name,
            new_name,
            namespace,
            storage_class,
            size_bytes,
            object_key,
            expected_checksum,
            bucket_namespace,
            bucket_secret_ref,
            bucket_endpoint,
            bucket_name,
            bucket_region,
            mode,
        } => {
            let k8s = require_k8s(k8s)?;
            // 1. Read the backup manifest back from RGW and verify its checksum (integrity check).
            let manifest_bytes = read_backup_manifest(
                &k8s,
                &bucket_namespace,
                &bucket_secret_ref,
                &bucket_endpoint,
                &bucket_region,
                &bucket_name,
                &object_key,
            )
            .await
            .ok();
            let manifest_verified = manifest_bytes
                .as_ref()
                .map(|b| sha256_hex(b) == expected_checksum)
                .unwrap_or(false);
            if !manifest_verified {
                tracing::warn!("backup {backup_id} manifest checksum mismatch/unreadable");
            }
            let manifest: serde_json::Value = manifest_bytes
                .as_ref()
                .and_then(|b| serde_json::from_slice(b).ok())
                .unwrap_or_else(|| serde_json::json!({}));

            // 2a. Data restore: create an empty PVC and apply the RBD diff from S3.
            if mode == "data" {
                let data_object = manifest
                    .pointer("/data/data_object")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        anyhow!("backup {backup_id} has no data object (not a data-mode backup)")
                    })?
                    .to_string();
                let data_checksum = manifest
                    .pointer("/data/data_checksum")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();

                let mut labels = std::collections::BTreeMap::new();
                labels.insert("zyvor.dev/volume-id".to_string(), new_volume_id.clone());
                labels.insert("zyvor.dev/restored-from".to_string(), backup_id.clone());
                let create = PvcCreateSpec {
                    name: new_name.clone(),
                    namespace: namespace.clone(),
                    storage_class: storage_class.clone(),
                    size_bytes,
                    access_modes: vec!["ReadWriteOnce".into()],
                    volume_mode: Some("Filesystem".into()),
                    labels,
                    data_source_snapshot: None,
                };
                // Idempotent retry: skip re-creating the PVC if a prior attempt already got it in
                // place (the k8s API errors on a name that already exists).
                if k8s
                    .get_pvc(&namespace, &new_name)
                    .await
                    .ok()
                    .flatten()
                    .is_none()
                {
                    k8s.create_pvc(&create)
                        .await
                        .with_context(|| format!("create restore PVC {namespace}/{new_name}"))?;
                }
                let phase = poll_pvc_phase(&k8s, &namespace, &new_name).await;
                if phase.as_deref() != Some("Bound") {
                    anyhow::bail!(
                        "restore PVC {namespace}/{new_name} did not bind (phase {phase:?})"
                    );
                }
                let (pool_name, image) = k8s
                    .resolve_rbd(&namespace, &new_name)
                    .await?
                    .ok_or_else(|| anyhow!("could not resolve RBD image for restored PVC"))?;

                // Stream the S3 data object straight into `rbd import-diff` — no in-memory buffer.
                let s3 = build_s3_target(
                    &k8s,
                    &bucket_namespace,
                    &bucket_secret_ref,
                    &bucket_endpoint,
                    &bucket_region,
                    &bucket_name,
                )
                .await?;
                let mut child = atlas_driver_ceph::rbd_import_diff_child(&pool_name, &image)
                    .map_err(|e| anyhow!("{e}"))?;
                let stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| anyhow!("rbd import-diff: no stdin"))?;
                let dl = s3.get_object_streaming(&data_object, stdin).await;
                let (imported_bytes, streamed_checksum) = match dl {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = child.kill().await;
                        return Err(e.context(format!("stream backup data {data_object}")));
                    }
                };
                // `stdin` was moved into get_object_streaming and dropped there, closing the pipe.
                let status = child.wait().await.context("wait for rbd import-diff")?;
                if !status.success() {
                    let mut err = String::new();
                    if let Some(mut se) = child.stderr.take() {
                        use tokio::io::AsyncReadExt;
                        let _ = se.read_to_string(&mut err).await;
                    }
                    anyhow::bail!("rbd import-diff into {pool_name}/{image}: {}", err.trim());
                }
                let data_verified = streamed_checksum == data_checksum;
                if !data_verified {
                    tracing::warn!("backup {backup_id} data checksum mismatch");
                }
                let imported_bytes = imported_bytes as usize;

                let vol = StorageVolume {
                    id: new_volume_id.clone(),
                    cluster_id: None,
                    pool_id: None,
                    name: new_name.clone(),
                    kind: VolumeKind::Block,
                    backend_native_id: Some(format!("pvc/{namespace}/{new_name}")),
                    size_bytes,
                    used_bytes: None,
                    state: "bound".into(),
                    health: Health::Ok,
                    kubernetes_namespace: Some(namespace.clone()),
                    pvc_name: Some(new_name.clone()),
                    storage_class_name: Some(storage_class.clone()),
                };
                atlas_inventory::upsert_volume(pool, &backend_id, tenant_id, &vol, None).await?;
                return Ok(serde_json::json!({
                    "volume_id": new_volume_id, "from_backup": backup_id, "mode": "data",
                    "manifest_verified": manifest_verified, "data_verified": data_verified,
                    "imported_bytes": imported_bytes, "rbd": format!("{pool_name}/{image}"),
                    "pvc": format!("{namespace}/{new_name}"), "bound": true
                }));
            }

            // 2b. Snapshot restore (default): provision a new volume from the backup's VolumeSnapshot.
            let phase = provision_from_snapshot(
                pool,
                &k8s,
                tenant_id,
                &backend_id,
                &new_volume_id,
                &snapshot_id,
                &snapshot_k8s_name,
                &new_name,
                &namespace,
                &storage_class,
                size_bytes,
                "ReadWriteOnce",
                "Filesystem",
                "restore-backup",
                None,
            )
            .await?;
            Ok(serde_json::json!({
                "volume_id": new_volume_id, "from_backup": backup_id, "mode": "snapshot",
                "manifest_verified": manifest_verified,
                "pvc": format!("{namespace}/{new_name}"),
                "phase": phase, "bound": phase.as_deref() == Some("Bound")
            }))
        }

        JobSpec::BackupDelete {
            backup_id,
            manifest_key,
            data_key,
            volume_namespace,
            pvc_name,
            rbd_snap,
            snapshot_name,
            bucket_namespace,
            bucket_secret_ref,
            bucket_endpoint,
            bucket_name,
            bucket_region,
        } => {
            let k8s = require_k8s(k8s)?;
            let secret = k8s
                .get_secret(&bucket_namespace, &bucket_secret_ref)
                .await?
                .ok_or_else(|| anyhow!("bucket secret {bucket_secret_ref} not found"))?;
            let access = secret
                .get("AWS_ACCESS_KEY_ID")
                .ok_or_else(|| anyhow!("bucket secret missing AWS_ACCESS_KEY_ID"))?;
            let secret_key = secret
                .get("AWS_SECRET_ACCESS_KEY")
                .ok_or_else(|| anyhow!("bucket secret missing AWS_SECRET_ACCESS_KEY"))?;
            let s3 = atlas_driver_rgw::S3Target::new(
                &bucket_endpoint,
                &bucket_region,
                &bucket_name,
                access,
                secret_key,
            )?;
            // Remove the S3 objects (idempotent).
            if let Err(e) = s3.delete_object(&manifest_key).await {
                tracing::warn!("delete manifest {manifest_key}: {e:#}");
            }
            if !data_key.is_empty() {
                if let Err(e) = s3.delete_object(&data_key).await {
                    tracing::warn!("delete data {data_key}: {e:#}");
                }
            }
            // Best-effort: remove the RBD snapshot if the source volume still resolves.
            if let Ok(Some((pool_name, image))) =
                k8s.resolve_rbd(&volume_namespace, &pvc_name).await
            {
                if let Err(e) = atlas_driver_ceph::rbd_snap_rm(&pool_name, &image, &rbd_snap).await
                {
                    tracing::warn!("rbd snap rm {pool_name}/{image}@{rbd_snap}: {e:#}");
                }
            }
            // Best-effort: remove the CSI VolumeSnapshot the backup created. Without this it's
            // orphaned forever, still holding the `pvc-as-source-protection` finalizer — the
            // source PVC then sits `Terminating` indefinitely the moment anyone tries to delete
            // it, even though both the backup and the volume delete report success. Verified
            // live: a backup's PVC was still stuck Terminating 17h after "successful" deletion.
            if let Err(e) = k8s
                .delete_volume_snapshot(&volume_namespace, &snapshot_name)
                .await
            {
                tracing::warn!("delete VolumeSnapshot {volume_namespace}/{snapshot_name}: {e:#}");
            }
            atlas_inventory::backups::delete_backup_row(pool, &backup_id).await?;
            Ok(serde_json::json!({ "backup_id": backup_id, "deleted": true }))
        }

        JobSpec::BucketDelete {
            bucket_id,
            namespace,
            obc_name,
        } => {
            let k8s = require_k8s(k8s)?;
            if let Err(e) = k8s.delete_obc(&namespace, &obc_name).await {
                tracing::warn!("delete OBC {namespace}/{obc_name}: {e}");
            }
            atlas_inventory::buckets::delete_bucket_row(pool, &bucket_id).await?;
            Ok(serde_json::json!({ "bucket_id": bucket_id, "deleted": true }))
        }

        JobSpec::BucketCreate {
            bucket_id,
            backend_id,
            namespace,
            obc_name,
            storage_class,
            max_objects,
            max_size,
            object_lock,
        } => {
            let k8s = require_k8s(k8s)?;
            let max_size = max_size.filter(|s| !s.is_empty());
            let mut additional_config = std::collections::BTreeMap::new();
            if let Some(n) = max_objects {
                additional_config.insert("maxObjects".to_string(), n.to_string());
            }
            if let Some(sz) = &max_size {
                additional_config.insert("maxSize".to_string(), sz.clone());
            }
            // Idempotent retry: skip re-creating the OBC if a prior attempt already bound one
            // (the k8s API errors on a name that already exists).
            if k8s
                .get_configmap(&namespace, &obc_name)
                .await
                .ok()
                .flatten()
                .is_none()
            {
                k8s.create_obc(&namespace, &obc_name, &storage_class, &additional_config)
                    .await
                    .with_context(|| format!("create OBC {namespace}/{obc_name}"))?;
            }

            // Only record the bucket in inventory once k8s has actually accepted the OBC create
            // (or a prior attempt already had) — inserting this earlier, from the HTTP handler
            // before the job even ran, left a permanent orphan row whenever OBC creation failed.
            atlas_inventory::buckets::insert_bucket(
                pool,
                &bucket_id,
                "global",
                &obc_name,
                &backend_id,
                &namespace,
                &obc_name,
                &storage_class,
            )
            .await?;

            // Rook writes a ConfigMap (same name) with BUCKET_* once the OBC binds.
            let cm = poll_configmap(&k8s, &namespace, &obc_name)
                .await
                .ok_or_else(|| anyhow!("OBC {obc_name} did not bind (no ConfigMap) in time"))?;
            let bucket_name = cm.get("BUCKET_NAME").cloned().unwrap_or_default();
            let host = cm.get("BUCKET_HOST").cloned().unwrap_or_default();
            let port = cm
                .get("BUCKET_PORT")
                .cloned()
                .unwrap_or_else(|| "80".into());
            let region = cm
                .get("BUCKET_REGION")
                .filter(|r| !r.is_empty())
                .cloned()
                .unwrap_or_else(|| "us-east-1".into());
            let endpoint = format!("http://{host}:{port}");
            // The OBC Secret (same name) holds the S3 credentials; store only the reference.
            atlas_inventory::buckets::set_bound(
                pool,
                &bucket_id,
                &bucket_name,
                &endpoint,
                &region,
                &obc_name,
            )
            .await?;

            // Enforce the RGW per-bucket quota directly (Rook may not apply OBC additionalConfig).
            let quota_set = if max_objects.is_some() || max_size.is_some() {
                match atlas_driver_ceph::radosgw_bucket_quota(
                    &bucket_name,
                    max_objects,
                    max_size.as_deref(),
                )
                .await
                {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!("set bucket quota on {bucket_name}: {e:#}");
                        false
                    }
                }
            } else {
                false
            };

            // Rook's ObjectBucketClaim has no object-lock field, and an unknown additionalConfig
            // key fails the claim. Object lock is the CreateBucket header, which cannot be added
            // later, so replace the empty bucket Rook just made with one created through the
            // signed S3 call (`create_bucket_with_object_lock`).
            if object_lock {
                if bucket_name.is_empty() {
                    anyhow::bail!(
                        "OBC {obc_name} bound without a bucket name; cannot enable object lock"
                    );
                }
                let s3 = build_s3_target(
                    &k8s,
                    &namespace,
                    &obc_name,
                    &endpoint,
                    &region,
                    &bucket_name,
                )
                .await?;
                s3.delete_bucket()
                    .await
                    .context("remove the unlocked RGW bucket before enabling object lock")?;
                s3.create_bucket_with_object_lock(true)
                    .await
                    .context("create the RGW bucket with S3 object lock")?;
            }
            Ok(serde_json::json!({
                "bucket_id": bucket_id, "bucket_name": bucket_name, "endpoint": endpoint,
                "quota_set": quota_set, "object_lock": object_lock
            }))
        }

        JobSpec::BucketCreateRustfs { .. } | JobSpec::BucketDeleteRustfs { .. } => {
            anyhow::bail!(
                "RustFS bucket jobs are retired; use Ceph RGW ObjectBucketClaim (bkd_ceph_lab)"
            )
        }

        JobSpec::BackupCreate {
            backup_id,
            snapshot_id,
            volume_namespace,
            pvc_name,
            snapshot_name,
            snapshot_class,
            object_key,
            manifest_json,
            bucket_namespace,
            bucket_secret_ref,
            bucket_endpoint,
            bucket_name,
            bucket_region,
            mode,
        } => {
            let k8s = require_k8s(k8s)?;
            // 1. point-in-time CSI snapshot of the source volume. Idempotent retry: skip
            // re-creating it if a prior attempt already put it in place (the k8s API errors on a
            // name that already exists).
            if !matches!(
                k8s.volume_snapshot_ready(&volume_namespace, &snapshot_name)
                    .await,
                Ok(Some(_))
            ) {
                k8s.create_volume_snapshot(
                    &volume_namespace,
                    &snapshot_name,
                    &pvc_name,
                    &snapshot_class,
                )
                .await
                .with_context(|| {
                    format!("snapshot {volume_namespace}/{snapshot_name} for backup")
                })?;
            }
            let ready = poll_snapshot_ready(&k8s, &volume_namespace, &snapshot_name).await;
            atlas_inventory::snapshots::set_state(
                pool,
                &snapshot_id,
                if ready { "ready" } else { "creating" },
            )
            .await?;

            // 2. read the bucket's S3 credentials in-cluster (never logged).
            let secret = k8s
                .get_secret(&bucket_namespace, &bucket_secret_ref)
                .await?
                .ok_or_else(|| anyhow!("bucket secret {bucket_secret_ref} not found"))?;
            let access = secret
                .get("AWS_ACCESS_KEY_ID")
                .ok_or_else(|| anyhow!("bucket secret missing AWS_ACCESS_KEY_ID"))?;
            let secret_key = secret
                .get("AWS_SECRET_ACCESS_KEY")
                .ok_or_else(|| anyhow!("bucket secret missing AWS_SECRET_ACCESS_KEY"))?;
            let s3 = atlas_driver_rgw::S3Target::new(
                &bucket_endpoint,
                &bucket_region,
                &bucket_name,
                access,
                secret_key,
            )?;

            // 3. (data mode) export the RBD image data and upload it to S3.
            let mut manifest: serde_json::Value =
                serde_json::from_str(&manifest_json).unwrap_or_else(|_| serde_json::json!({}));
            let mut data_summary = serde_json::Value::Null;
            if mode == "data" {
                // Stream `rbd export-diff` straight into an S3 multipart upload — no in-memory cap.
                const PART_SIZE: usize = 16 * 1024 * 1024;
                let (pool_name, image) = k8s
                    .resolve_rbd(&volume_namespace, &pvc_name)
                    .await?
                    .ok_or_else(|| {
                        anyhow!("could not resolve RBD image for {volume_namespace}/{pvc_name}")
                    })?;
                let rbd_snap = format!("atlasbkp-{}", &backup_id[4..]);
                // Idempotent retry: skip re-creating the snapshot if a prior attempt already made
                // it (e.g. the upload below failed after this succeeded) — `rbd snap create`
                // errors on a name that already exists.
                let existing_snaps = atlas_driver_ceph::rbd_snap_list(&pool_name, &image)
                    .await
                    .unwrap_or_default();
                if !existing_snaps.iter().any(|s| s == &rbd_snap) {
                    atlas_driver_ceph::rbd_snap_create(&pool_name, &image, &rbd_snap)
                        .await
                        .with_context(|| format!("rbd snap {pool_name}/{image}@{rbd_snap}"))?;
                }
                let data_key = format!("{object_key}.rbd-diff");
                let mut child =
                    atlas_driver_ceph::rbd_export_diff_child(&pool_name, &image, &rbd_snap)
                        .map_err(|e| anyhow!("{e}"))?;
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| anyhow!("rbd export-diff: no stdout"))?;
                let upload = s3
                    .put_multipart_streaming(&data_key, PART_SIZE, stdout)
                    .await;
                let (data_bytes, data_checksum) = match upload {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = child.kill().await;
                        return Err(e.context(format!("stream backup data {data_key}")));
                    }
                };
                let status = child.wait().await.context("wait for rbd export-diff")?;
                if !status.success() {
                    let mut err = String::new();
                    if let Some(mut se) = child.stderr.take() {
                        use tokio::io::AsyncReadExt;
                        let _ = se.read_to_string(&mut err).await;
                    }
                    anyhow::bail!(
                        "rbd export-diff {pool_name}/{image}@{rbd_snap}: {}",
                        err.trim()
                    );
                }
                data_summary = serde_json::json!({
                    "data_object": data_key, "data_bytes": data_bytes,
                    "data_checksum": data_checksum, "format": "rbd-export-diff",
                    "rbd": format!("{pool_name}/{image}@{rbd_snap}")
                });
                manifest["data"] = data_summary.clone();
                manifest["format"] = serde_json::json!("rbd-export-diff");
            }

            // 4. write the manifest object to RGW, then read it back to verify.
            let manifest_bytes = serde_json::to_vec(&manifest)?;
            // Always the *manifest's* checksum — restore's read-back verification (create_restore
            // in routes/object_store.rs) compares the fetched manifest against backups.checksum,
            // so storing the data checksum here (as this used to do for mode=="data") made that
            // comparison — sha256(manifest bytes) vs the RBD data's hash — fail on every single
            // data-mode restore, logging a false "manifest checksum mismatch" for a manifest that
            // was never actually corrupt. The data's own checksum is independently verified via
            // the manifest's embedded data.data_checksum field, which create_restore already
            // checks separately (its "data_verified" result) — nothing needs it here too.
            let checksum = sha256_hex(&manifest_bytes);
            s3.put_object(&object_key, manifest_bytes.clone())
                .await
                .with_context(|| format!("PUT backup manifest {object_key}"))?;
            let verified =
                matches!(s3.get_object(&object_key).await, Ok(got) if got == manifest_bytes);
            // The row was inserted with format="manifest-v1" regardless of mode; reflect the real
            // format now that we know whether data was actually streamed, so the UI can tell
            // manifest-only backups apart from full rbd-export-diff ones.
            let format = if mode == "data" {
                Some("rbd-export-diff")
            } else {
                None
            };
            atlas_inventory::backups::set_state(
                pool,
                &backup_id,
                if verified { "verified" } else { "completed" },
                Some(&checksum),
                format,
            )
            .await?;
            Ok(serde_json::json!({
                "backup_id": backup_id, "object_key": object_key, "mode": mode,
                "checksum": checksum, "verified": verified, "snapshot_ready": ready,
                "data": data_summary
            }))
        }
        _ => anyhow::bail!("not an object spec"),
    }
}
