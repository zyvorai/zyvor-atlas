// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Rook (`ceph.rook.io/v1`) CR helpers built on the generic `apply_cr`/`get_cr_status`/`list_crs`
//! mechanism in `lib.rs`. Returns raw `serde_json::Value`/plain structs like the rest of that
//! generic layer — no typed CRD codegen dependency, since Rook's CR schemas are wide and this
//! only needs a handful of fields (`status.phase`, `status.ceph.health`).
//!
//! This is a second, precise source of truth alongside `atlas-driver-ceph`'s `ceph`/`rbd` CLI
//! path — additive, not a replacement: the CLI path still works where the k8s driver isn't
//! available at all.

use crate::{K8sDriver, K8sError};
use kube::core::DynamicObject;
use std::collections::HashMap;

pub const ROOK_GROUP: &str = "ceph.rook.io";
pub const ROOK_VERSION: &str = "v1";

fn phase_of(obj: &DynamicObject) -> Option<String> {
    obj.data
        .get("status")?
        .get("phase")?
        .as_str()
        .map(|s| s.to_string())
}

/// A pod reduced to the fields a status view needs.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PodBrief {
    pub name: String,
    pub phase: Option<String>,
    pub ready: bool,
    pub node: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NvmeofStorageClass {
    pub name: String,
    pub provisioner: String,
    pub parameters: std::collections::BTreeMap<String, String>,
}

/// Media class of one entry from Rook's `local-device-<node>` ConfigMap: `nvme` when the kernel
/// name (or real path) is an `nvme*` device, `hdd` when Rook reports `rotational: true`, else
/// `ssd`. Rook's discovery schema uses `name`, `kernel-name`, `real-path` and `rotational`.
pub fn classify_device(dev: &serde_json::Value) -> &'static str {
    let is_nvme = ["name", "kernel-name", "real-path"].iter().any(|k| {
        dev.get(*k)
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.trim_start_matches("/dev/").starts_with("nvme"))
    });
    if is_nvme {
        "nvme"
    } else if dev.get("rotational").and_then(|v| v.as_bool()) == Some(true) {
        "hdd"
    } else {
        "ssd"
    }
}

fn with_device_class(mut dev: serde_json::Value) -> serde_json::Value {
    let class = classify_device(&dev);
    if let Some(obj) = dev.as_object_mut() {
        obj.insert("class".into(), class.into());
    }
    dev
}

fn name_of(obj: &DynamicObject) -> String {
    obj.metadata.name.clone().unwrap_or_default()
}

/// Name + `status.phase` of a Rook CR — the compact view list endpoints need.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RookCrStatus {
    pub name: String,
    pub phase: Option<String>,
}

/// Precise pool classification derived from live Rook CRs (see `classify_pool_via_rook`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RookPoolKind {
    Rbd,
    CephfsData,
    CephfsMetadata,
    Rgw,
}

impl RookPoolKind {
    /// The exact string values `atlas-driver-ceph::real::pool_kind_from_name` already produces,
    /// so callers can drop this straight into `StoragePool::kind` without a translation layer.
    pub fn as_str(&self) -> &'static str {
        match self {
            RookPoolKind::Rbd => "rbd",
            RookPoolKind::CephfsData => "cephfs_data",
            RookPoolKind::CephfsMetadata => "cephfs_metadata",
            RookPoolKind::Rgw => "rgw",
        }
    }
}

impl K8sDriver {
    /// `CephCluster.status` (`.ceph.health`, `.phase`, `.storage.deviceClasses`, ...).
    pub async fn get_ceph_cluster_status(
        &self,
        ns: &str,
        name: &str,
    ) -> Result<Option<serde_json::Value>, K8sError> {
        self.get_cr_status(ROOK_GROUP, ROOK_VERSION, "CephCluster", ns, name)
            .await
    }

    /// Every `CephBlockPool` CR in `ns`, with its `status.phase`.
    pub async fn list_ceph_block_pools(&self, ns: &str) -> Result<Vec<RookCrStatus>, K8sError> {
        let items = self
            .list_crs(ROOK_GROUP, ROOK_VERSION, "CephBlockPool", ns)
            .await?;
        Ok(items
            .iter()
            .map(|o| RookCrStatus {
                name: name_of(o),
                phase: phase_of(o),
            })
            .collect())
    }

    /// Every `CephFilesystem` CR in `ns`, with its `status.phase`.
    pub async fn list_ceph_filesystems(&self, ns: &str) -> Result<Vec<RookCrStatus>, K8sError> {
        let items = self
            .list_crs(ROOK_GROUP, ROOK_VERSION, "CephFilesystem", ns)
            .await?;
        Ok(items
            .iter()
            .map(|o| RookCrStatus {
                name: name_of(o),
                phase: phase_of(o),
            })
            .collect())
    }

    /// Every `CephObjectStore` CR in `ns`, with its `status.phase`.
    pub async fn list_ceph_object_stores(&self, ns: &str) -> Result<Vec<RookCrStatus>, K8sError> {
        let items = self
            .list_crs(ROOK_GROUP, ROOK_VERSION, "CephObjectStore", ns)
            .await?;
        Ok(items
            .iter()
            .map(|o| RookCrStatus {
                name: name_of(o),
                phase: phase_of(o),
            })
            .collect())
    }

    /// Build the exact set of pool names Rook itself will have created, mapped to their precise
    /// kind — an ahead-of-time replacement for the name-heuristic fallback in
    /// `atlas-driver-ceph::real::pool_kind_from_name`. Mirrors the `RbdOwners` pattern
    /// (`rbd_image_owners`): built once per discovery pass from cluster CRs and handed to the
    /// discovery enrichment step, so `atlas-discovery` stays decoupled from the Kubernetes driver.
    ///
    /// Naming convention (Rook v1.20): a `CephBlockPool` CR's `metadata.name` *is* the pool name;
    /// a `CephFilesystem` named `fs` creates `fs-metadata` (metadata pool) and `fs-<dataPool.name>`
    /// (each entry in `spec.dataPools`); a `CephObjectStore` named `store` creates `store.rgw.*`
    /// pools (control/meta/log/buckets.index/buckets.data/buckets.non-ec/otp).
    pub async fn known_rook_pool_kinds(
        &self,
        ns: &str,
    ) -> Result<HashMap<String, RookPoolKind>, K8sError> {
        let mut out = HashMap::new();

        for p in self
            .list_crs(ROOK_GROUP, ROOK_VERSION, "CephBlockPool", ns)
            .await?
        {
            out.insert(name_of(&p), RookPoolKind::Rbd);
        }

        for fs in self
            .list_crs(ROOK_GROUP, ROOK_VERSION, "CephFilesystem", ns)
            .await?
        {
            let fs_name = name_of(&fs);
            out.insert(format!("{fs_name}-metadata"), RookPoolKind::CephfsMetadata);
            if let Some(data_pools) = fs
                .data
                .pointer("/spec/dataPools")
                .and_then(|v| v.as_array())
            {
                for dp in data_pools {
                    if let Some(dp_name) = dp.get("name").and_then(|v| v.as_str()) {
                        out.insert(format!("{fs_name}-{dp_name}"), RookPoolKind::CephfsData);
                    }
                }
            }
        }

        for os in self
            .list_crs(ROOK_GROUP, ROOK_VERSION, "CephObjectStore", ns)
            .await?
        {
            let os_name = name_of(&os);
            for suffix in [
                "control",
                "meta",
                "log",
                "buckets.index",
                "buckets.data",
                "buckets.non-ec",
                "otp",
            ] {
                out.insert(format!("{os_name}.rgw.{suffix}"), RookPoolKind::Rgw);
            }
        }

        Ok(out)
    }

    /// Classify a single pool name via `known_rook_pool_kinds`. `None` means no Rook CR claims
    /// this pool — the caller should fall back to the old name heuristic (e.g. non-Rook Ceph).
    pub async fn classify_pool_via_rook(
        &self,
        ns: &str,
        pool_name: &str,
    ) -> Result<Option<RookPoolKind>, K8sError> {
        Ok(self
            .known_rook_pool_kinds(ns)
            .await?
            .get(pool_name)
            .copied())
    }

    /// Rook's own device-discovery ConfigMap for one node (`local-device-<node>`, populated by
    /// Rook's discovery DaemonSet when `ROOK_ENABLE_DISCOVERY_DAEMON` is on). `None` means the
    /// ConfigMap doesn't exist — the caller must fail closed (never assume a device is safe when
    /// this oracle is unavailable), not silently skip the check. Rook's schema is a single
    /// `"devices"` data key holding a JSON-encoded array of `{name, filesystem, empty, ...}`.
    pub async fn get_local_devices(
        &self,
        ns: &str,
        node_name: &str,
    ) -> Result<Option<Vec<serde_json::Value>>, K8sError> {
        let Some(cm) = self.get_configmap(ns, &format!("local-device-{node_name}")).await? else {
            return Ok(None);
        };
        let Some(raw) = cm.get("devices") else {
            return Ok(Some(Vec::new()));
        };
        Ok(Some(serde_json::from_str(raw).unwrap_or_default()))
    }

    /// Every `CephNVMeOFGateway` CR in `ns` as plain JSON (`metadata`, `spec`, `status`). `None`
    /// means the CRD is not installed (HTTP 404 on the list), which is different from "installed,
    /// none created": Rook added the CRD in v1.19 and marks it experimental.
    pub async fn list_nvmeof_gateways(
        &self,
        ns: &str,
    ) -> Result<Option<Vec<serde_json::Value>>, K8sError> {
        match self.list_crs(ROOK_GROUP, ROOK_VERSION, "CephNVMeOFGateway", ns).await {
            Ok(items) => Ok(Some(
                items
                    .into_iter()
                    .map(|o| serde_json::to_value(o).unwrap_or_default())
                    .collect(),
            )),
            Err(K8sError::Api(kube::Error::Api(e))) if e.code == 404 => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Pods in `ns` matching a label selector, reduced to what a status view needs.
    pub async fn list_pods_by_label(
        &self,
        ns: &str,
        selector: &str,
    ) -> Result<Vec<PodBrief>, K8sError> {
        use k8s_openapi::api::core::v1::Pod;
        let api: kube::Api<Pod> = kube::Api::namespaced(self.client.clone(), ns);
        let list = api.list(&kube::api::ListParams::default().labels(selector)).await?;
        Ok(list
            .items
            .into_iter()
            .map(|p| {
                let status = p.status.unwrap_or_default();
                PodBrief {
                    name: p.metadata.name.unwrap_or_default(),
                    phase: status.phase,
                    ready: status
                        .conditions
                        .unwrap_or_default()
                        .iter()
                        .any(|c| c.type_ == "Ready" && c.status == "True"),
                    node: p.spec.and_then(|s| s.node_name),
                }
            })
            .collect())
    }

    /// StorageClasses served by the Ceph-CSI NVMe-oF driver (`*nvmeof.csi.ceph.com`), with their
    /// parameters (`subsystemNQN`, `listeners`, ...). Parameters that name credentials
    /// (`csi.storage.k8s.io/*`) are left out; a PVC from one of these classes is what creates an
    /// NVMe namespace.
    pub async fn list_nvmeof_storage_classes(&self) -> Result<Vec<NvmeofStorageClass>, K8sError> {
        use k8s_openapi::api::storage::v1::StorageClass;
        let api: kube::Api<StorageClass> = kube::Api::all(self.client.clone());
        let list = api.list(&kube::api::ListParams::default()).await?;
        Ok(list
            .items
            .into_iter()
            .filter(|sc| sc.provisioner.ends_with("nvmeof.csi.ceph.com"))
            .map(|sc| NvmeofStorageClass {
                name: sc.metadata.name.unwrap_or_default(),
                provisioner: sc.provisioner,
                parameters: sc
                    .parameters
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(k, _)| !k.starts_with("csi.storage.k8s.io/"))
                    .collect(),
            })
            .collect())
    }

    /// Like [`Self::get_local_devices`], but each entry gains a `"class"` field
    /// (`nvme`/`ssd`/`hdd`, see [`classify_device`]). Other fields pass through untouched.
    pub async fn get_local_devices_classified(
        &self,
        ns: &str,
        node_name: &str,
    ) -> Result<Option<Vec<serde_json::Value>>, K8sError> {
        Ok(self
            .get_local_devices(ns, node_name)
            .await?
            .map(|devs| devs.into_iter().map(with_device_class).collect()))
    }

    /// Whether Rook's discovery data reports `device_basename` (bare name, e.g. `sdb`) on
    /// `node_name` as genuinely empty (no filesystem, no partitions) — the safety oracle for the
    /// raw-disk-provisioning path, since the gateway itself has no remote-exec mechanism to shell
    /// `lsblk`/`findmnt` against an arbitrary Kubernetes node the way it can for a local ZFS host.
    /// `Err` means the device wasn't found in Rook's own discovery data at all.
    pub async fn ceph_device_reports_empty(
        &self,
        ns: &str,
        node_name: &str,
        device_basename: &str,
    ) -> Result<bool, K8sError> {
        let devices = self.get_local_devices(ns, node_name).await?.ok_or_else(|| {
            K8sError::NotFound(format!(
                "Rook device discovery ConfigMap local-device-{node_name} not found in {ns}"
            ))
        })?;
        let entry = devices
            .iter()
            .find(|d| d.get("name").and_then(|n| n.as_str()) == Some(device_basename))
            .ok_or_else(|| {
                K8sError::NotFound(format!(
                    "device {device_basename} not visible to Rook's discovery on node {node_name}"
                ))
            })?;
        let empty = entry.get("empty").and_then(|v| v.as_bool()).unwrap_or(false);
        let has_fs = entry
            .get("filesystem")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty());
        Ok(empty && !has_fs)
    }

    /// `CephCluster.spec.storage` — just the fields the raw-disk-provisioning path needs
    /// (`useAllDevices` and the explicit per-node device list). `None` if the CR itself is absent.
    pub async fn get_ceph_cluster_storage(
        &self,
        ns: &str,
        name: &str,
    ) -> Result<Option<CephClusterStorageSpec>, K8sError> {
        let spec = self.get_cr_spec(ROOK_GROUP, ROOK_VERSION, "CephCluster", ns, name).await?;
        Ok(spec.map(|s| {
            s.get("storage")
                .cloned()
                .and_then(|storage| serde_json::from_value(storage).ok())
                .unwrap_or_default()
        }))
    }

    /// Add `device_basename` (bare name, e.g. `sdb` — **not** `/dev/sdb`; Rook's own CR schema
    /// expects the bare name) to `node_name`'s entry in `spec.storage.nodes[].devices`, creating
    /// the node entry if it doesn't exist yet. Idempotent: a no-op if the device is already
    /// listed, so a retried invocation of the job that calls this is safe to re-run. Issues a
    /// scoped JSON Merge Patch touching only `spec.storage.nodes` (via `patch_cr_merge`) — never a
    /// full-spec replace, which would clobber `mon`/`dashboard`/`network`/`cephVersion` etc.
    ///
    /// Concurrency: safe today because the job engine has exactly one worker serializing every
    /// job system-wide, so two Atlas-driven calls can never race each other here — only a
    /// concurrent manual `kubectl edit cephcluster` is a residual risk, accepted (as `apply_cr`
    /// already accepts the same class of race for pool/filesystem/objectstore CRs). If the job
    /// engine is ever sharded to multiple workers, this needs its own compare-and-swap.
    pub async fn add_ceph_cluster_device(
        &self,
        ns: &str,
        cluster_name: &str,
        node_name: &str,
        device_basename: &str,
        opts: &CephDeviceOptions,
    ) -> Result<(), K8sError> {
        // If the CephCluster CR is genuinely absent, `storage` defaults to empty and the
        // `patch_cr_merge` call below fails honestly with a real (404) API error — this never
        // fabricates success against a cluster that doesn't exist.
        let mut storage = self
            .get_ceph_cluster_storage(ns, cluster_name)
            .await?
            .unwrap_or_default();
        if !upsert_device(&mut storage, node_name, device_basename, opts) {
            return Ok(()); // already listed — nothing to do
        }
        let patch = serde_json::json!({ "spec": { "storage": { "nodes": storage.nodes } } });
        self.patch_cr_merge(ROOK_GROUP, ROOK_VERSION, "CephCluster", ns, cluster_name, patch)
            .await
    }
}

/// `CephCluster.spec.storage` — just the fields the raw-disk-provisioning path reads/writes.
/// Deserialized loosely (`#[serde(default)]` everywhere) since the full Rook storage spec has many
/// more fields (`volumeClaimTemplates`, `onlyApplyOSDPlacement`, ...) this doesn't need to model.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CephClusterStorageSpec {
    #[serde(default)]
    pub use_all_nodes: bool,
    #[serde(default)]
    pub use_all_devices: bool,
    #[serde(default)]
    pub nodes: Vec<CephClusterNodeSpec>,
}

/// One node entry of `spec.storage.nodes`. Everything Atlas doesn't model (`config`,
/// `deviceFilter`, `volumeClaimTemplates`, ...) is kept in `extra` and written back unchanged:
/// the merge patch replaces the whole `nodes` array, so dropping a field here would silently
/// delete it from the live CR.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct CephClusterNodeSpec {
    pub name: String,
    #[serde(default)]
    pub devices: Vec<CephClusterDeviceSpec>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One entry of a node's `devices`. `config` is Rook's per-device string map (`metadataDevice`,
/// `databaseSizeMB`, `walSizeMB`, `osdsPerDevice`, `deviceClass`, ...); unmodelled sibling fields
/// round-trip through `extra` for the same reason as on [`CephClusterNodeSpec`].
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct CephClusterDeviceSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub config: std::collections::BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// BlueStore layout options for a newly claimed device. All unset means Rook's defaults (DB and
/// WAL co-located on the data device).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct CephDeviceOptions {
    /// Bare name (`nvme1n1`) of the shared device that holds the OSD's RocksDB (and WAL).
    pub metadata_device: Option<String>,
    pub database_size_mb: Option<u64>,
    pub wal_size_mb: Option<u64>,
    pub osds_per_device: Option<u32>,
}

impl CephDeviceOptions {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Rook's `config` map: every value is a string.
    pub fn to_config(&self) -> std::collections::BTreeMap<String, String> {
        let mut c = std::collections::BTreeMap::new();
        if let Some(d) = &self.metadata_device {
            c.insert("metadataDevice".into(), d.clone());
        }
        if let Some(v) = self.database_size_mb {
            c.insert("databaseSizeMB".into(), v.to_string());
        }
        if let Some(v) = self.wal_size_mb {
            c.insert("walSizeMB".into(), v.to_string());
        }
        if let Some(v) = self.osds_per_device {
            c.insert("osdsPerDevice".into(), v.to_string());
        }
        c
    }
}

/// Add `device` with `opts` to `node` in `storage`, creating the node entry if needed. Returns
/// `false` — leaving `storage` untouched — when the device is already listed: that is what makes
/// a retried job a no-op, and it never rewrites the layout of a device Rook may already have
/// turned into an OSD.
pub fn upsert_device(
    storage: &mut CephClusterStorageSpec,
    node: &str,
    device: &str,
    opts: &CephDeviceOptions,
) -> bool {
    let entry = CephClusterDeviceSpec {
        name: device.to_string(),
        config: opts.to_config(),
        extra: Default::default(),
    };
    match storage.nodes.iter_mut().find(|n| n.name == node) {
        Some(n) if n.devices.iter().any(|d| d.name == device) => false,
        Some(n) => {
            n.devices.push(entry);
            true
        }
        None => {
            storage.nodes.push(CephClusterNodeSpec {
                name: node.to_string(),
                devices: vec![entry],
                extra: Default::default(),
            });
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_spec_reads_rooks_camel_case_keys() {
        let st: CephClusterStorageSpec = serde_json::from_value(serde_json::json!({
            "useAllNodes": true, "useAllDevices": true
        }))
        .unwrap();
        assert!(st.use_all_nodes && st.use_all_devices);
    }

    #[test]
    fn options_become_rooks_string_config_map() {
        assert!(CephDeviceOptions::default().to_config().is_empty());
        let c = CephDeviceOptions {
            metadata_device: Some("nvme1n1".into()),
            database_size_mb: Some(61440),
            wal_size_mb: Some(2048),
            osds_per_device: Some(2),
        }
        .to_config();
        assert_eq!(c["metadataDevice"], "nvme1n1");
        assert_eq!(c["databaseSizeMB"], "61440");
        assert_eq!(c["walSizeMB"], "2048");
        assert_eq!(c["osdsPerDevice"], "2");
    }

    #[test]
    fn upsert_keeps_fields_atlas_does_not_model() {
        let mut st: CephClusterStorageSpec = serde_json::from_value(serde_json::json!({
            "nodes": [{
                "name": "n1",
                "config": { "osdsPerDevice": "1" },
                "devices": [{ "name": "sdb", "config": { "deviceClass": "ssd" }, "fullpath": "/dev/sdb" }]
            }]
        }))
        .unwrap();
        let opts = CephDeviceOptions { metadata_device: Some("nvme1n1".into()), ..Default::default() };
        assert!(upsert_device(&mut st, "n1", "sdc", &opts));
        let out = serde_json::to_value(&st.nodes).unwrap();
        assert_eq!(out[0]["config"]["osdsPerDevice"], "1", "node-level config survived");
        assert_eq!(out[0]["devices"][0]["config"]["deviceClass"], "ssd");
        assert_eq!(out[0]["devices"][0]["fullpath"], "/dev/sdb", "unmodelled field survived");
        assert_eq!(out[0]["devices"][1]["config"]["metadataDevice"], "nvme1n1");
        // A plain device serializes without an empty config map.
        assert!(upsert_device(&mut st, "n2", "sdb", &CephDeviceOptions::default()));
        let out = serde_json::to_value(&st.nodes).unwrap();
        assert!(out[1]["devices"][0].get("config").is_none());
    }

    #[test]
    fn upsert_is_a_noop_for_an_already_listed_device() {
        let mut st = CephClusterStorageSpec::default();
        assert!(upsert_device(&mut st, "n1", "sdb", &CephDeviceOptions::default()));
        let again = CephDeviceOptions { metadata_device: Some("nvme1n1".into()), ..Default::default() };
        assert!(!upsert_device(&mut st, "n1", "sdb", &again));
        assert!(st.nodes[0].devices[0].config.is_empty(), "existing layout is never rewritten");
    }

    #[test]
    fn classifies_device_media() {
        use serde_json::json;
        assert_eq!(classify_device(&json!({"name": "nvme0n1", "rotational": false})), "nvme");
        assert_eq!(classify_device(&json!({"name": "sdb", "real-path": "/dev/nvme1n1"})), "nvme");
        assert_eq!(classify_device(&json!({"name": "sdc", "rotational": true})), "hdd");
        assert_eq!(classify_device(&json!({"name": "sdd", "rotational": false})), "ssd");
        assert_eq!(classify_device(&json!({"name": "sde"})), "ssd");
        let tagged = with_device_class(json!({"name": "sdc", "rotational": true, "empty": true}));
        assert_eq!(tagged["class"], "hdd");
        assert_eq!(tagged["empty"], true);
    }

    fn obj_with_status(name: &str, phase: &str) -> DynamicObject {
        let mut obj = DynamicObject::new(
            name,
            &kube::core::ApiResource::from_gvk(&kube::core::GroupVersionKind::gvk(
                ROOK_GROUP,
                ROOK_VERSION,
                "CephBlockPool",
            )),
        );
        obj.data = serde_json::json!({ "status": { "phase": phase } });
        obj
    }

    #[test]
    fn phase_of_reads_status_phase() {
        let obj = obj_with_status("rbd-nvme-prod", "Ready");
        assert_eq!(phase_of(&obj), Some("Ready".to_string()));
    }

    #[test]
    fn phase_of_none_when_no_status() {
        let obj = DynamicObject::new(
            "x",
            &kube::core::ApiResource::from_gvk(&kube::core::GroupVersionKind::gvk(
                ROOK_GROUP,
                ROOK_VERSION,
                "CephBlockPool",
            )),
        );
        assert_eq!(phase_of(&obj), None);
    }
}
