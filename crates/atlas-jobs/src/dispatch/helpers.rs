// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use atlas_api_types::{Health, StorageVolume, VolumeKind};
use atlas_driver_k8s::{K8sDriver, PvcCreateSpec};
use sqlx::AnyPool;

use crate::spec::OwnerRef;

pub(crate) const BIND_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Build an `S3Target` for a bucket, reading its credentials from the Rook OBC Secret in-cluster
/// (the keys are never logged or returned).
pub(crate) async fn build_s3_target(
    k8s: &K8sDriver,
    bucket_namespace: &str,
    bucket_secret_ref: &str,
    bucket_endpoint: &str,
    bucket_region: &str,
    bucket_name: &str,
) -> Result<atlas_driver_rgw::S3Target> {
    let secret = k8s
        .get_secret(bucket_namespace, bucket_secret_ref)
        .await?
        .ok_or_else(|| anyhow!("bucket secret {bucket_secret_ref} not found"))?;
    let access = secret
        .get("AWS_ACCESS_KEY_ID")
        .ok_or_else(|| anyhow!("bucket secret missing AWS_ACCESS_KEY_ID"))?;
    let secret_key = secret
        .get("AWS_SECRET_ACCESS_KEY")
        .ok_or_else(|| anyhow!("bucket secret missing AWS_SECRET_ACCESS_KEY"))?;
    atlas_driver_rgw::S3Target::new(
        bucket_endpoint,
        bucket_region,
        bucket_name,
        access,
        secret_key,
    )
}

/// Resolve a backend row's `connection_ref` (a Kubernetes Secret name holding
/// `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`) into an `S3Target` — the bring-your-own-S3 analogue of
/// `build_s3_target` above, for backends with one shared backend-level service credential rather
/// than a per-bucket auto-provisioned Secret (a bring-your-own S3 endpoint has no ObjectBucketClaim-style operator).
/// Also returns the resolved `connection_ref` itself, since callers need it verbatim to record as
/// the bucket row's `secret_ref` (so a later object-level request can re-resolve credentials the
/// same way `bucket_s3_target` already does for RGW buckets).
pub(crate) async fn build_s3_target_for_backend(
    k8s: &K8sDriver,
    pool: &AnyPool,
    backend_id: &str,
    credentials_namespace: &str,
    endpoint: &str,
    region: &str,
    bucket_name: &str,
) -> Result<(atlas_driver_rgw::S3Target, String)> {
    let backend = atlas_inventory::get_backend(pool, backend_id)
        .await?
        .ok_or_else(|| anyhow!("backend {backend_id} not registered"))?;
    let connection_ref = backend.connection_ref.ok_or_else(|| {
        anyhow!("backend {backend_id} has no connection_ref (credentials secret) configured")
    })?;
    let target = build_s3_target(
        k8s,
        credentials_namespace,
        &connection_ref,
        endpoint,
        region,
        bucket_name,
    )
    .await?;
    Ok((target, connection_ref))
}

/// Read a backup manifest object back from RGW (used by restore to verify integrity).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn read_backup_manifest(
    k8s: &K8sDriver,
    bucket_namespace: &str,
    bucket_secret_ref: &str,
    bucket_endpoint: &str,
    bucket_region: &str,
    bucket_name: &str,
    object_key: &str,
) -> Result<Vec<u8>> {
    let s3 = build_s3_target(
        k8s,
        bucket_namespace,
        bucket_secret_ref,
        bucket_endpoint,
        bucket_region,
        bucket_name,
    )
    .await?;
    s3.get_object(object_key).await
}

/// Provision a new PVC populated from a VolumeSnapshot, record the volume, and protect the source
/// snapshot. Shared by clone/restore and backup-restore. Returns the PVC phase.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn provision_from_snapshot(
    pool: &AnyPool,
    k8s: &K8sDriver,
    tenant_id: &str,
    backend_id: &str,
    new_volume_id: &str,
    snapshot_id: &str,
    snapshot_k8s_name: &str,
    new_name: &str,
    namespace: &str,
    storage_class: &str,
    size_bytes: i64,
    access_mode: &str,
    volume_mode: &str,
    provenance: &str,
    owner: Option<OwnerRef>,
) -> Result<Option<String>> {
    let mut labels = std::collections::BTreeMap::new();
    labels.insert("zyvor.dev/volume-id".to_string(), new_volume_id.to_string());
    labels.insert(
        "zyvor.dev/from-snapshot".to_string(),
        snapshot_id.to_string(),
    );
    labels.insert("zyvor.dev/provenance".to_string(), provenance.to_string());
    if let Some(o) = &owner {
        labels.insert("zyvor.dev/owner-product".to_string(), o.product.clone());
    }
    let create = PvcCreateSpec {
        name: new_name.to_string(),
        namespace: namespace.to_string(),
        storage_class: storage_class.to_string(),
        size_bytes,
        access_modes: vec![access_mode.to_string()],
        volume_mode: Some(volume_mode.to_string()),
        labels,
        data_source_snapshot: Some(snapshot_k8s_name.to_string()),
    };
    // Idempotent retry: skip re-creating the PVC if a prior attempt already got it in place (the
    // k8s API errors on a name that already exists).
    if k8s
        .get_pvc(namespace, new_name)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        k8s.create_pvc(&create)
            .await
            .with_context(|| format!("{provenance} PVC {namespace}/{new_name} from snapshot"))?;
    }

    let phase = poll_pvc_phase(k8s, namespace, new_name).await;
    let vol = StorageVolume {
        id: new_volume_id.to_string(),
        cluster_id: None,
        pool_id: None,
        name: new_name.to_string(),
        kind: VolumeKind::Block,
        backend_native_id: Some(format!("pvc/{namespace}/{new_name}")),
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
        kubernetes_namespace: Some(namespace.to_string()),
        pvc_name: Some(new_name.to_string()),
        storage_class_name: Some(storage_class.to_string()),
    };
    atlas_inventory::upsert_volume(pool, backend_id, tenant_id, &vol, None).await?;
    atlas_inventory::set_volume_source_snapshot(pool, new_volume_id, snapshot_id).await?;
    atlas_inventory::snapshots::set_protected(pool, snapshot_id, true).await?;
    if let Some(o) = owner {
        atlas_inventory::insert_binding(
            pool,
            &format!("bind_{new_volume_id}"),
            tenant_id,
            &o.product,
            &o.resource_type,
            &o.resource_id,
            "volume",
            new_volume_id,
            &o.role,
        )
        .await?;
    }
    Ok(phase)
}

/// Poll for an OBC's output ConfigMap (present once the bucket is bound).
pub(crate) async fn poll_configmap(
    k8s: &K8sDriver,
    ns: &str,
    name: &str,
) -> Option<std::collections::BTreeMap<String, String>> {
    let deadline = std::time::Instant::now() + BIND_TIMEOUT;
    loop {
        if let Ok(Some(cm)) = k8s.get_configmap(ns, name).await {
            if cm.contains_key("BUCKET_NAME") {
                return Some(cm);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

pub(crate) fn require_k8s(k8s: &Option<Arc<K8sDriver>>) -> Result<Arc<K8sDriver>> {
    k8s.clone()
        .ok_or_else(|| anyhow!("no Kubernetes cluster is attached; cannot run the write path"))
}

pub(crate) async fn poll_pvc_phase(k8s: &K8sDriver, ns: &str, name: &str) -> Option<String> {
    let deadline = std::time::Instant::now() + BIND_TIMEOUT;
    loop {
        if let Ok(Some(pvc)) = k8s.get_pvc(ns, name).await {
            if pvc.phase.as_deref() == Some("Bound") {
                return Some("Bound".into());
            }
            if std::time::Instant::now() >= deadline {
                return pvc.phase;
            }
        } else if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

pub(crate) async fn poll_snapshot_ready(k8s: &K8sDriver, ns: &str, name: &str) -> bool {
    let deadline = std::time::Instant::now() + BIND_TIMEOUT;
    loop {
        if let Ok(Some(true)) = k8s.volume_snapshot_ready(ns, name).await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Poll a Rook (or any) custom resource's `status.phase` until it reaches `"Ready"` or the bind
/// timeout elapses. Returns the last-seen phase (`None` if the CR never got a status at all).
pub(crate) async fn poll_cr_ready(
    k8s: &K8sDriver,
    group: &str,
    version: &str,
    kind: &str,
    ns: &str,
    name: &str,
) -> Option<String> {
    let deadline = std::time::Instant::now() + BIND_TIMEOUT;
    loop {
        let phase = k8s
            .get_cr_status(group, version, kind, ns, name)
            .await
            .ok()
            .flatten()
            .and_then(|s| {
                s.get("phase")
                    .and_then(|p| p.as_str())
                    .map(|p| p.to_string())
            });
        if phase.as_deref() == Some("Ready") {
            return phase;
        }
        if std::time::Instant::now() >= deadline {
            return phase;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

pub(crate) fn parse_kind(s: &str) -> VolumeKind {
    match s {
        "filesystem" => VolumeKind::Filesystem,
        "object" => VolumeKind::Object,
        _ => VolumeKind::Block,
    }
}
