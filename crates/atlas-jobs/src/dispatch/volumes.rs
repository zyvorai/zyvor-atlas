// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
use anyhow::{Context, Result};
use atlas_api_types::{Health, StorageVolume};
use atlas_driver_k8s::{K8sDriver, PvcCreateSpec};
use sqlx::AnyPool;
use std::sync::Arc;

use super::helpers::{
    parse_kind, poll_pvc_phase, poll_snapshot_ready, provision_from_snapshot, require_k8s,
};
use crate::spec::JobSpec;

pub(crate) async fn dispatch_volumes(
    pool: &AnyPool,
    k8s: &Option<Arc<K8sDriver>>,
    tenant_id: &str,
    spec: JobSpec,
) -> Result<serde_json::Value> {
    match spec {
        JobSpec::VolumeCreate {
            volume_id,
            backend_id,
            name,
            namespace,
            storage_class,
            access_mode,
            volume_mode,
            size_bytes,
            kind,
            policy,
            owner,
        } => {
            let k8s = require_k8s(k8s)?;
            let mut labels = std::collections::BTreeMap::new();
            labels.insert("zyvor.dev/volume-id".to_string(), volume_id.clone());
            if let Some(o) = &owner {
                labels.insert("zyvor.dev/owner-product".to_string(), o.product.clone());
            }
            let create = PvcCreateSpec {
                name: name.clone(),
                namespace: namespace.clone(),
                storage_class: storage_class.clone(),
                size_bytes,
                access_modes: vec![access_mode],
                volume_mode: Some(volume_mode),
                labels,
                data_source_snapshot: None,
            };
            // Idempotent retry: if a prior attempt already created the PVC (e.g. dispatch
            // succeeded but the job's mark-succeeded write failed, forcing a full re-run), the
            // k8s API errors on a PVC that already exists — skip it and just verify/record.
            if k8s
                .get_pvc(&namespace, &name)
                .await
                .ok()
                .flatten()
                .is_none()
            {
                k8s.create_pvc(&create)
                    .await
                    .with_context(|| format!("create PVC {namespace}/{name}"))?;
            }

            // Verify: poll for Bound (Immediate SCs bind quickly; WaitForFirstConsumer stays Pending).
            let phase = poll_pvc_phase(&k8s, &namespace, &name).await;
            // Resolve the real Ceph RBD `pool/image` (via the bound PV's CSI
            // attributes) so products that attach the volume directly (e.g. host
            // libvirt / a hypervisor VM disk) get a usable backend id. Falls back
            // to a PVC reference when it isn't a bound ceph-csi RBD volume.
            let native = if phase.as_deref() == Some("Bound") {
                match k8s.resolve_rbd(&namespace, &name).await {
                    // Match atlas-driver-ceph::real::list_volumes's `rbd:{pool}/{image}` format —
                    // a mismatch here (this used to omit the prefix) meant a fresh discovery pass
                    // could never recognize this row as the same real image by backend_native_id.
                    Ok(Some((pool, image))) => format!("rbd:{pool}/{image}"),
                    _ => format!("pvc/{namespace}/{name}"),
                }
            } else {
                format!("pvc/{namespace}/{name}")
            };
            let vol = StorageVolume {
                id: volume_id.clone(),
                cluster_id: None,
                pool_id: None,
                name: name.clone(),
                kind: parse_kind(&kind),
                backend_native_id: Some(native),
                size_bytes,
                used_bytes: None,
                state: phase
                    .clone()
                    .unwrap_or_else(|| "provisioning".into())
                    .to_lowercase(),
                health: if phase.as_deref() == Some("Bound") {
                    Health::Ok
                } else {
                    Health::Unknown
                },
                kubernetes_namespace: Some(namespace.clone()),
                pvc_name: Some(name.clone()),
                storage_class_name: Some(storage_class),
            };
            atlas_inventory::upsert_volume(pool, &backend_id, tenant_id, &vol, policy.as_deref())
                .await?;

            if let Some(o) = owner {
                atlas_inventory::insert_binding(
                    pool,
                    &format!("bind_{volume_id}"),
                    tenant_id,
                    &o.product,
                    &o.resource_type,
                    &o.resource_id,
                    "volume",
                    &volume_id,
                    &o.role,
                )
                .await?;
            }
            Ok(serde_json::json!({
                "volume_id": volume_id, "pvc": format!("{namespace}/{name}"),
                "phase": phase, "bound": phase.as_deref() == Some("Bound")
            }))
        }

        JobSpec::VolumeDelete {
            volume_id,
            namespace,
            pvc_name,
        } => {
            let k8s = require_k8s(k8s)?;
            atlas_inventory::set_volume_state(pool, &volume_id, "deleting").await?;
            // Deleting an absent PVC is not an error (idempotent).
            if let Err(e) = k8s.delete_pvc(&namespace, &pvc_name).await {
                tracing::warn!("delete_pvc {namespace}/{pvc_name}: {e}");
            }
            atlas_inventory::delete_volume_row(pool, &volume_id).await?;
            Ok(serde_json::json!({ "volume_id": volume_id, "deleted": true }))
        }

        JobSpec::VolumeExpand {
            volume_id,
            namespace,
            pvc_name,
            new_size_bytes,
        } => {
            let k8s = require_k8s(k8s)?;
            k8s.expand_pvc(&namespace, &pvc_name, new_size_bytes)
                .await
                .with_context(|| format!("expand PVC {namespace}/{pvc_name}"))?;
            sqlx::query("UPDATE storage_volumes SET size_bytes=$1, updated_at=$2 WHERE id=$3")
                .bind(new_size_bytes)
                .bind(atlas_inventory::now_rfc3339(chrono::Utc::now()))
                .bind(&volume_id)
                .execute(pool)
                .await?;
            Ok(serde_json::json!({ "volume_id": volume_id, "size_bytes": new_size_bytes }))
        }

        JobSpec::SnapshotCreate {
            snapshot_id,
            volume_id,
            name,
            namespace,
            pvc_name,
            snapshot_class,
        } => {
            let k8s = require_k8s(k8s)?;
            // Idempotent retry: only insert the tracking row once — `insert_snapshot` is a plain
            // INSERT, so re-running it after a prior attempt already got this far (e.g. dispatch
            // succeeded but the job's mark-succeeded write failed, forcing a full re-run) would
            // hit a UNIQUE constraint and fail forever.
            let already_tracked = atlas_inventory::snapshots::get_snapshot(pool, &snapshot_id)
                .await?
                .is_some();
            if !already_tracked {
                atlas_inventory::snapshots::insert_snapshot(
                    pool,
                    &snapshot_id,
                    tenant_id,
                    &volume_id,
                    &name,
                    None,
                    "crash",
                    "creating",
                )
                .await?;
            }
            // Likewise, skip re-creating the VolumeSnapshot if a prior attempt already put it in
            // place — the k8s API errors on a name that already exists.
            if !matches!(
                k8s.volume_snapshot_ready(&namespace, &name).await,
                Ok(Some(_))
            ) {
                k8s.create_volume_snapshot(&namespace, &name, &pvc_name, &snapshot_class)
                    .await
                    .with_context(|| format!("create VolumeSnapshot {namespace}/{name}"))?;
            }
            let ready = poll_snapshot_ready(&k8s, &namespace, &name).await;
            atlas_inventory::snapshots::set_state(
                pool,
                &snapshot_id,
                if ready { "ready" } else { "creating" },
            )
            .await?;
            Ok(serde_json::json!({ "snapshot_id": snapshot_id, "ready": ready }))
        }

        JobSpec::SnapshotDelete {
            snapshot_id,
            namespace,
            name,
        } => {
            let k8s = require_k8s(k8s)?;
            if let Err(e) = k8s.delete_volume_snapshot(&namespace, &name).await {
                tracing::warn!("delete VolumeSnapshot {namespace}/{name}: {e}");
            }
            atlas_inventory::snapshots::delete_snapshot_row(pool, &snapshot_id).await?;
            Ok(serde_json::json!({ "snapshot_id": snapshot_id, "deleted": true }))
        }

        JobSpec::SnapshotClone {
            mode,
            new_volume_id,
            backend_id,
            snapshot_id,
            snapshot_k8s_name,
            new_name,
            namespace,
            storage_class,
            size_bytes,
            access_mode,
            volume_mode,
            owner,
        } => {
            let k8s = require_k8s(k8s)?;
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
                &access_mode,
                &volume_mode,
                &mode,
                owner,
            )
            .await?;
            Ok(serde_json::json!({
                "volume_id": new_volume_id, "from_snapshot": snapshot_id, "mode": mode,
                "pvc": format!("{namespace}/{new_name}"),
                "phase": phase, "bound": phase.as_deref() == Some("Bound")
            }))
        }
        _ => anyhow::bail!("not a volume spec"),
    }
}
