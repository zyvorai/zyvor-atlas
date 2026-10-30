// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Rook lifecycle jobs: create/delete a `CephBlockPool`/`CephFilesystem`/`CephObjectStore` CR plus
//! its matching StorageClass, via the generic `apply_cr`/`delete_cr`/`apply_storage_class` k8s
//! mechanism — the same one CloudNativePG/Percona/Strimzi CRs already go through
//! (`atlas_driver_k8s::K8sDriver`). CR shapes mirror the static manifests in
//! `deploy/rook-ceph-lab/{03-rbd-blockpool,04-cephfs,05-rgw-objectstore}*.yaml`.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use atlas_driver_k8s::K8sDriver;
use sqlx::AnyPool;

use super::helpers::{poll_cr_ready, require_k8s};
use crate::spec::JobSpec;

const ROOK_GROUP: &str = "ceph.rook.io";
const ROOK_VERSION: &str = "v1";

fn csi_secret_params(prefix: &str, ns: &str) -> Vec<(String, String)> {
    [
        "provisioner-secret-name",
        "provisioner-secret-namespace",
        "controller-expand-secret-name",
        "controller-expand-secret-namespace",
        "node-stage-secret-name",
        "node-stage-secret-namespace",
    ]
    .iter()
    .map(|suffix| {
        let key = format!("csi.storage.k8s.io/{suffix}");
        let value = if suffix.ends_with("namespace") {
            ns.to_string()
        } else {
            format!(
                "rook-csi-{prefix}-{}",
                if suffix.starts_with("node") {
                    "node"
                } else {
                    "provisioner"
                }
            )
        };
        (key, value)
    })
    .collect()
}

/// Scan `ceph device ls -f json` output for an entry on `node_name`/`device_basename` that already
/// has at least one OSD daemon attached — i.e. Rook has actually finished claiming the device, not
/// just accepted the CR patch.
fn osd_found_for_device(devices: &serde_json::Value, node_name: &str, device_basename: &str) -> bool {
    let Some(entries) = devices.as_array() else {
        return false;
    };
    entries.iter().any(|entry| {
        let has_osd_daemon = entry
            .get("daemons")
            .and_then(|d| d.as_array())
            .is_some_and(|daemons| {
                daemons
                    .iter()
                    .any(|d| d.as_str().is_some_and(|s| s.starts_with("osd.")))
            });
        if !has_osd_daemon {
            return false;
        }
        entry
            .get("location")
            .and_then(|l| l.as_array())
            .is_some_and(|locations| {
                locations.iter().any(|loc| {
                    let host_matches = loc.get("host").and_then(|h| h.as_str()) == Some(node_name);
                    let dev_matches = loc
                        .get("dev")
                        .and_then(|d| d.as_str())
                        .is_some_and(|d| d.trim_start_matches("/dev/") == device_basename);
                    host_matches && dev_matches
                })
            })
    })
}

pub(crate) async fn dispatch_rook(
    _pool: &AnyPool,
    k8s: &Option<Arc<K8sDriver>>,
    _tenant_id: &str,
    spec: JobSpec,
) -> Result<serde_json::Value> {
    match spec {
        JobSpec::CephPoolCreate {
            name,
            namespace,
            storage_class,
            replicated_size,
            failure_domain,
            device_class,
        } => {
            let k8s = require_k8s(k8s)?;
            let mut cr_spec = serde_json::json!({
                "failureDomain": failure_domain,
                "replicated": { "size": replicated_size, "requireSafeReplicaSize": false },
            });
            if let Some(dc) = &device_class {
                cr_spec["deviceClass"] = serde_json::json!(dc);
            }
            k8s.apply_cr(
                ROOK_GROUP,
                ROOK_VERSION,
                "CephBlockPool",
                &namespace,
                &name,
                cr_spec,
            )
            .await?;

            let mut params: BTreeMap<String, String> = BTreeMap::new();
            params.insert("clusterID".into(), namespace.clone());
            params.insert("pool".into(), name.clone());
            params.insert("imageFormat".into(), "2".into());
            params.insert(
                "imageFeatures".into(),
                "layering,fast-diff,object-map,deep-flatten,exclusive-lock".into(),
            );
            params.insert("csi.storage.k8s.io/fstype".into(), "ext4".into());
            for (k, v) in csi_secret_params("rbd", &namespace) {
                params.insert(k, v);
            }
            let mut labels = BTreeMap::new();
            labels.insert("zyvor.dev/storage-backend".into(), "ceph".into());
            labels.insert("zyvor.dev/storage-kind".into(), "block".into());
            k8s.apply_storage_class(
                &storage_class,
                "rook-ceph.rbd.csi.ceph.com",
                &params,
                &labels,
                true,
            )
            .await?;

            let phase = poll_cr_ready(
                &k8s,
                ROOK_GROUP,
                ROOK_VERSION,
                "CephBlockPool",
                &namespace,
                &name,
            )
            .await;
            Ok(serde_json::json!({
                "pool": name, "storage_class": storage_class, "phase": phase,
                "ready": phase.as_deref() == Some("Ready")
            }))
        }

        JobSpec::CephPoolDelete {
            name,
            namespace,
            storage_class,
        } => {
            let k8s = require_k8s(k8s)?;
            k8s.delete_storage_class(&storage_class).await?;
            k8s.delete_cr(ROOK_GROUP, ROOK_VERSION, "CephBlockPool", &namespace, &name)
                .await?;
            Ok(serde_json::json!({ "pool": name, "deleted": true }))
        }

        JobSpec::CephFilesystemCreate {
            name,
            namespace,
            storage_class,
            data_pool_name,
            replicated_size,
        } => {
            let k8s = require_k8s(k8s)?;
            let replicated =
                serde_json::json!({ "size": replicated_size, "requireSafeReplicaSize": false });
            let cr_spec = serde_json::json!({
                "metadataPool": { "replicated": replicated.clone() },
                "dataPools": [{ "name": data_pool_name, "replicated": replicated }],
                "metadataServer": { "activeCount": 1, "activeStandby": true },
            });
            k8s.apply_cr(
                ROOK_GROUP,
                ROOK_VERSION,
                "CephFilesystem",
                &namespace,
                &name,
                cr_spec,
            )
            .await?;

            let mut params: BTreeMap<String, String> = BTreeMap::new();
            params.insert("clusterID".into(), namespace.clone());
            params.insert("fsName".into(), name.clone());
            params.insert("pool".into(), format!("{name}-{data_pool_name}"));
            for (k, v) in csi_secret_params("cephfs", &namespace) {
                params.insert(k, v);
            }
            let mut labels = BTreeMap::new();
            labels.insert("zyvor.dev/storage-backend".into(), "ceph".into());
            labels.insert("zyvor.dev/storage-kind".into(), "file".into());
            labels.insert("zyvor.dev/access-mode".into(), "rwx".into());
            k8s.apply_storage_class(
                &storage_class,
                "rook-ceph.cephfs.csi.ceph.com",
                &params,
                &labels,
                true,
            )
            .await?;

            let phase = poll_cr_ready(
                &k8s,
                ROOK_GROUP,
                ROOK_VERSION,
                "CephFilesystem",
                &namespace,
                &name,
            )
            .await;
            Ok(serde_json::json!({
                "filesystem": name, "storage_class": storage_class, "phase": phase,
                "ready": phase.as_deref() == Some("Ready")
            }))
        }

        JobSpec::CephFilesystemDelete {
            name,
            namespace,
            storage_class,
        } => {
            let k8s = require_k8s(k8s)?;
            k8s.delete_storage_class(&storage_class).await?;
            k8s.delete_cr(
                ROOK_GROUP,
                ROOK_VERSION,
                "CephFilesystem",
                &namespace,
                &name,
            )
            .await?;
            Ok(serde_json::json!({ "filesystem": name, "deleted": true }))
        }

        JobSpec::CephObjectStoreCreate {
            name,
            namespace,
            storage_class,
            replicated_size,
            gateway_port,
            gateway_instances,
        } => {
            let k8s = require_k8s(k8s)?;
            let replicated =
                serde_json::json!({ "size": replicated_size, "requireSafeReplicaSize": false });
            let cr_spec = serde_json::json!({
                "metadataPool": { "failureDomain": "host", "replicated": replicated.clone() },
                "dataPool": { "failureDomain": "host", "replicated": replicated },
                "preservePoolsOnDelete": false,
                "gateway": { "port": gateway_port, "instances": gateway_instances },
            });
            k8s.apply_cr(
                ROOK_GROUP,
                ROOK_VERSION,
                "CephObjectStore",
                &namespace,
                &name,
                cr_spec,
            )
            .await?;

            let mut params: BTreeMap<String, String> = BTreeMap::new();
            params.insert("objectStoreName".into(), name.clone());
            params.insert("objectStoreNamespace".into(), namespace.clone());
            let mut labels = BTreeMap::new();
            labels.insert("zyvor.dev/storage-backend".into(), "ceph".into());
            labels.insert("zyvor.dev/storage-kind".into(), "object".into());
            k8s.apply_storage_class(
                &storage_class,
                "rook-ceph.ceph.rook.io/bucket",
                &params,
                &labels,
                false,
            )
            .await?;

            let phase = poll_cr_ready(
                &k8s,
                ROOK_GROUP,
                ROOK_VERSION,
                "CephObjectStore",
                &namespace,
                &name,
            )
            .await;
            Ok(serde_json::json!({
                "object_store": name, "storage_class": storage_class, "phase": phase,
                "ready": phase.as_deref() == Some("Ready")
            }))
        }

        JobSpec::CephObjectStoreDelete {
            name,
            namespace,
            storage_class,
        } => {
            let k8s = require_k8s(k8s)?;
            k8s.delete_storage_class(&storage_class).await?;
            k8s.delete_cr(
                ROOK_GROUP,
                ROOK_VERSION,
                "CephObjectStore",
                &namespace,
                &name,
            )
            .await?;
            Ok(serde_json::json!({ "object_store": name, "deleted": true }))
        }

        JobSpec::CephOsdAddDevice {
            namespace,
            cluster_name,
            node_name,
            device_path,
            confirmed_device_path,
            db_device,
            db_size_mb,
            wal_size_mb,
            osds_per_device,
        } => {
            anyhow::ensure!(
                device_path == confirmed_device_path,
                "device path confirmation mismatch"
            );
            let k8s = require_k8s(k8s)?;
            let device_basename = device_path.trim_start_matches("/dev/").to_string();

            // Idempotent — safe to re-run on every retry: no-ops if the device is already listed.
            let opts = atlas_driver_k8s::rook::CephDeviceOptions {
                metadata_device: db_device
                    .as_deref()
                    .map(|d| d.trim_start_matches("/dev/").to_string()),
                database_size_mb: db_size_mb,
                wal_size_mb,
                osds_per_device,
            };
            k8s.add_ceph_cluster_device(
                &namespace,
                &cluster_name,
                &node_name,
                &device_basename,
                &opts,
            )
            .await?;

            // One-shot check, not a blocking loop: is there NOW a Ceph OSD backed by this exact
            // device on this exact host? Rook's reconciliation can take minutes and this job
            // engine has exactly one worker serializing every job system-wide — blocking it that
            // long would stall every other tenant's jobs. Instead, this dispatch call returns
            // immediately either way, letting the *existing* retry/backoff machinery in
            // `engine.rs::run_worker` act as the poll loop (an `Err` here is automatically
            // rescheduled with exponential backoff up to the job's configured retry budget).
            // `ceph device ls` is Ceph's own ground truth of daemon<->device mapping — stronger
            // than the CephCluster's aggregate `status.phase`, which can already read "Ready"
            // while this specific device's `ceph-volume prepare` is still mid-flight.
            let devices = atlas_driver_ceph::ceph_cmd(&["device", "ls"]).await?;
            if osd_found_for_device(&devices, &node_name, &device_basename) {
                return Ok(serde_json::json!({
                    "node": node_name, "device": device_path, "ready": true
                }));
            }
            anyhow::bail!(
                "rook has not yet produced an OSD for {device_path} on {node_name} — will retry"
            )
        }

        _ => anyhow::bail!("not a rook spec"),
    }
}
