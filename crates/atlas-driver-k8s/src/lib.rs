// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Live Kubernetes driver (read-only in MVP slice 1).
//!
//! Surfaces `StorageClass`, `PersistentVolumeClaim`, and `PersistentVolume` objects so the gateway
//! can serve `/storage-classes` etc. straight from the cluster. Modeled on
//! `veyron/src/kube/vm_data_disk.rs` (`Api::<StorageClass>::all(...).list(...)`).
//!
//! `create_pvc` is defined but returns `NotImplemented` until MVP slice 2 (the write path).

use atlas_api_types::StorageClassInfo;
use k8s_openapi::api::core::v1::{
    ConfigMap, PersistentVolume, PersistentVolumeClaim, PersistentVolumeClaimSpec, Secret,
    TypedLocalObjectReference, VolumeResourceRequirements,
};
use k8s_openapi::api::storage::v1::StorageClass;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::{Api, Client};
use std::collections::BTreeMap;

pub mod node_disks;
pub mod rook;

/// Request to create a PVC (the MVP write path).
#[derive(Debug, Clone)]
pub struct PvcCreateSpec {
    pub name: String,
    pub namespace: String,
    pub storage_class: String,
    pub size_bytes: i64,
    pub access_modes: Vec<String>,
    pub volume_mode: Option<String>,
    pub labels: BTreeMap<String, String>,
    /// When set, the PVC is populated from this VolumeSnapshot (clone/restore).
    pub data_source_snapshot: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum K8sError {
    #[error("kube client init failed: {0}")]
    Init(String),
    #[error("kube api error: {0}")]
    Api(#[from] kube::Error),
    #[error("not found: {0}")]
    NotFound(String),
}

/// Ceph CSI provisioners we badge as Ceph-backed (PDF §7.1).
const CEPH_PROVISIONERS: &[&str] = &[
    "rbd.csi.ceph.com",
    "cephfs.csi.ceph.com",
    "rook-ceph.rbd.csi.ceph.com",
    "rook-ceph.cephfs.csi.ceph.com",
];

fn is_ceph_provisioner(p: &str) -> bool {
    CEPH_PROVISIONERS.iter().any(|c| p.ends_with(c))
}

/// List every item of `api`, following the `continue` token so results aren't silently truncated
/// at the page `limit` on clusters with more objects than fit in one page.
async fn list_all<K>(api: &Api<K>, page_limit: u32) -> Result<Vec<K>, kube::Error>
where
    K: Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let mut items = Vec::new();
    let mut lp = ListParams::default().limit(page_limit);
    loop {
        let list = api.list(&lp).await?;
        let cont = list.metadata.continue_.clone();
        items.extend(list.items);
        match cont {
            Some(token) if !token.is_empty() => lp = lp.continue_token(&token),
            _ => break,
        }
    }
    Ok(items)
}

/// Thin wrapper over a `kube::Client` exposing read-only storage discovery.
#[derive(Clone)]
pub struct K8sDriver {
    client: Client,
}

impl K8sDriver {
    /// Build from the ambient kubeconfig / in-cluster config (honors `KUBECONFIG`).
    pub async fn try_default() -> Result<Self, K8sError> {
        let client = Client::try_default()
            .await
            .map_err(|e| K8sError::Init(e.to_string()))?;
        Ok(Self { client })
    }

    /// List all StorageClasses, tagging Ceph-backed ones.
    pub async fn list_storage_classes(&self) -> Result<Vec<StorageClassInfo>, K8sError> {
        let api: Api<StorageClass> = Api::all(self.client.clone());
        let items = list_all(&api, 500).await?;
        Ok(items
            .into_iter()
            .map(|sc| {
                let provisioner = sc.provisioner;
                StorageClassInfo {
                    is_ceph: is_ceph_provisioner(&provisioner),
                    backend_type: (provisioner == "driver.longhorn.io").then(|| "longhorn".to_string())
                        .or_else(|| is_ceph_provisioner(&provisioner).then(|| "ceph".to_string())),
                    name: sc.metadata.name.unwrap_or_default(),
                    provisioner,
                    reclaim_policy: sc.reclaim_policy,
                    volume_binding_mode: sc.volume_binding_mode,
                    allow_volume_expansion: sc.allow_volume_expansion,
                    labels: sc.metadata.labels.unwrap_or_default().into_iter().collect(),
                }
            })
            .collect())
    }

    /// List PVCs across all namespaces (or one namespace when `namespace` is `Some`).
    pub async fn list_pvcs(&self, namespace: Option<&str>) -> Result<Vec<PvcSummary>, K8sError> {
        let api: Api<PersistentVolumeClaim> = match namespace {
            Some(ns) => Api::namespaced(self.client.clone(), ns),
            None => Api::all(self.client.clone()),
        };
        let items = list_all(&api, 500).await?;
        Ok(items.into_iter().map(PvcSummary::from).collect())
    }

    /// List cluster PersistentVolumes.
    pub async fn list_pvs(&self) -> Result<Vec<PvSummary>, K8sError> {
        let api: Api<PersistentVolume> = Api::all(self.client.clone());
        let items = list_all(&api, 500).await?;
        Ok(items.into_iter().map(PvSummary::from).collect())
    }

    /// Create-or-replace a StorageClass. Used by the Rook pool/filesystem/object-store lifecycle
    /// jobs (`atlas-jobs::dispatch::rook`) to pair a freshly-applied Rook CR with the StorageClass
    /// pointing CSI at it, mirroring the static pairs in `deploy/rook-ceph-lab/*.yaml`.
    pub async fn apply_storage_class(
        &self,
        name: &str,
        provisioner: &str,
        parameters: &BTreeMap<String, String>,
        labels: &BTreeMap<String, String>,
        allow_volume_expansion: bool,
    ) -> Result<(), K8sError> {
        let api: Api<StorageClass> = Api::all(self.client.clone());
        let sc = StorageClass {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                labels: (!labels.is_empty()).then(|| labels.clone()),
                ..Default::default()
            },
            provisioner: provisioner.to_string(),
            parameters: (!parameters.is_empty()).then(|| parameters.clone()),
            reclaim_policy: Some("Delete".to_string()),
            allow_volume_expansion: Some(allow_volume_expansion),
            volume_binding_mode: Some("Immediate".to_string()),
            ..Default::default()
        };
        match api.get_opt(name).await? {
            Some(existing) => {
                let mut sc = sc;
                sc.metadata.resource_version = existing.metadata.resource_version;
                api.replace(name, &PostParams::default(), &sc).await?;
            }
            None => {
                api.create(&PostParams::default(), &sc).await?;
            }
        }
        Ok(())
    }

    /// Delete a StorageClass (ignores not-found).
    pub async fn delete_storage_class(&self, name: &str) -> Result<(), K8sError> {
        let api: Api<StorageClass> = Api::all(self.client.clone());
        match api.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    // ---- write path (slice 2) ----

    fn pvc_api(&self, ns: &str) -> Api<PersistentVolumeClaim> {
        Api::namespaced(self.client.clone(), ns)
    }

    /// Create a PVC. Returns its summary (initially Pending until bound).
    pub async fn create_pvc(&self, spec: &PvcCreateSpec) -> Result<PvcSummary, K8sError> {
        let access_modes = if spec.access_modes.is_empty() {
            vec!["ReadWriteOnce".to_string()]
        } else {
            spec.access_modes.clone()
        };
        let mut requests = std::collections::BTreeMap::new();
        requests.insert("storage".to_string(), Quantity(spec.size_bytes.to_string()));

        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(spec.name.clone()),
                namespace: Some(spec.namespace.clone()),
                labels: (!spec.labels.is_empty()).then(|| spec.labels.clone()),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(access_modes),
                storage_class_name: Some(spec.storage_class.clone()),
                volume_mode: spec.volume_mode.clone(),
                resources: Some(VolumeResourceRequirements {
                    requests: Some(requests),
                    ..Default::default()
                }),
                // Clone/restore: populate from a VolumeSnapshot.
                data_source: spec.data_source_snapshot.as_ref().map(|snap| {
                    TypedLocalObjectReference {
                        api_group: Some("snapshot.storage.k8s.io".to_string()),
                        kind: "VolumeSnapshot".to_string(),
                        name: snap.clone(),
                    }
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let created = self
            .pvc_api(&spec.namespace)
            .create(&PostParams::default(), &pvc)
            .await?;
        Ok(PvcSummary::from(created))
    }

    /// Fetch a PVC summary (None if it doesn't exist).
    pub async fn get_pvc(&self, ns: &str, name: &str) -> Result<Option<PvcSummary>, K8sError> {
        match self.pvc_api(ns).get_opt(name).await? {
            Some(p) => Ok(Some(PvcSummary::from(p))),
            None => Ok(None),
        }
    }

    /// Delete a PVC.
    pub async fn delete_pvc(&self, ns: &str, name: &str) -> Result<(), K8sError> {
        self.pvc_api(ns)
            .delete(name, &DeleteParams::default())
            .await?;
        Ok(())
    }

    /// Expand a PVC to a larger size (merge-patch `spec.resources.requests.storage`).
    pub async fn expand_pvc(&self, ns: &str, name: &str, new_bytes: i64) -> Result<(), K8sError> {
        let patch = serde_json::json!({
            "spec": { "resources": { "requests": { "storage": new_bytes.to_string() } } }
        });
        self.pvc_api(ns)
            .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        Ok(())
    }

    fn volume_snapshot_api(&self, ns: &str) -> (Api<DynamicObject>, ApiResource) {
        let gvk = GroupVersionKind::gvk("snapshot.storage.k8s.io", "v1", "VolumeSnapshot");
        let ar = ApiResource::from_gvk(&gvk);
        (Api::namespaced_with(self.client.clone(), ns, &ar), ar)
    }

    /// Create a VolumeSnapshot from a source PVC. Returns the snapshot object name.
    pub async fn create_volume_snapshot(
        &self,
        ns: &str,
        name: &str,
        source_pvc: &str,
        snapshot_class: &str,
    ) -> Result<String, K8sError> {
        let (api, ar) = self.volume_snapshot_api(ns);
        let mut obj = DynamicObject::new(name, &ar);
        obj.metadata.namespace = Some(ns.to_string());
        obj.data = serde_json::json!({
            "spec": {
                "volumeSnapshotClassName": snapshot_class,
                "source": { "persistentVolumeClaimName": source_pvc }
            }
        });
        let created = api.create(&PostParams::default(), &obj).await?;
        Ok(created.metadata.name.unwrap_or_else(|| name.to_string()))
    }

    /// Whether a VolumeSnapshot reports `status.readyToUse` (None if not yet reported / absent).
    pub async fn volume_snapshot_ready(
        &self,
        ns: &str,
        name: &str,
    ) -> Result<Option<bool>, K8sError> {
        let (api, _ar) = self.volume_snapshot_api(ns);
        match api.get_opt(name).await? {
            Some(obj) => Ok(obj
                .data
                .get("status")
                .and_then(|s| s.get("readyToUse"))
                .and_then(|v| v.as_bool())),
            None => Ok(None),
        }
    }

    /// Delete a VolumeSnapshot.
    pub async fn delete_volume_snapshot(&self, ns: &str, name: &str) -> Result<(), K8sError> {
        let (api, _ar) = self.volume_snapshot_api(ns);
        api.delete(name, &DeleteParams::default()).await?;
        Ok(())
    }

    // ---- generic custom-resource apply/status/delete (CloudNativePG, MySQL operator, Debezium) ----

    fn cr_api(&self, group: &str, version: &str, kind: &str, ns: &str) -> Api<DynamicObject> {
        let gvk = GroupVersionKind::gvk(group, version, kind);
        let ar = ApiResource::from_gvk(&gvk);
        Api::namespaced_with(self.client.clone(), ns, &ar)
    }

    /// Create-or-replace an arbitrary namespaced custom resource from a `spec` JSON object. Used to
    /// apply operator CRs (CloudNativePG `Cluster`, MySQL `PerconaXtraDBCluster`, Debezium connectors).
    pub async fn apply_cr(
        &self,
        group: &str,
        version: &str,
        kind: &str,
        ns: &str,
        name: &str,
        spec: serde_json::Value,
    ) -> Result<(), K8sError> {
        self.apply_cr_labeled(group, version, kind, ns, name, &BTreeMap::new(), spec)
            .await
    }

    /// Like `apply_cr` but also stamps `metadata.labels` — needed e.g. for Strimzi `KafkaConnector`,
    /// which associates with its Connect cluster via the `strimzi.io/cluster` label.
    #[allow(clippy::too_many_arguments)]
    pub async fn apply_cr_labeled(
        &self,
        group: &str,
        version: &str,
        kind: &str,
        ns: &str,
        name: &str,
        labels: &BTreeMap<String, String>,
        spec: serde_json::Value,
    ) -> Result<(), K8sError> {
        self.apply_cr_meta(
            group,
            version,
            kind,
            ns,
            name,
            labels,
            &BTreeMap::new(),
            spec,
        )
        .await
    }

    /// Like `apply_cr_labeled` but also stamps `metadata.annotations` — needed e.g. for Strimzi
    /// `KafkaConnect`, which only reconciles `KafkaConnector` CRs when annotated with
    /// `strimzi.io/use-connector-resources: "true"`.
    #[allow(clippy::too_many_arguments)]
    pub async fn apply_cr_meta(
        &self,
        group: &str,
        version: &str,
        kind: &str,
        ns: &str,
        name: &str,
        labels: &BTreeMap<String, String>,
        annotations: &BTreeMap<String, String>,
        spec: serde_json::Value,
    ) -> Result<(), K8sError> {
        let gvk = GroupVersionKind::gvk(group, version, kind);
        let ar = ApiResource::from_gvk(&gvk);
        let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), ns, &ar);
        let mut obj = DynamicObject::new(name, &ar);
        obj.metadata.namespace = Some(ns.to_string());
        if !labels.is_empty() {
            obj.metadata.labels = Some(labels.clone().into_iter().collect());
        }
        if !annotations.is_empty() {
            obj.metadata.annotations = Some(annotations.clone().into_iter().collect());
        }
        obj.data = serde_json::json!({ "spec": spec });
        match api.get_opt(name).await? {
            Some(existing) => {
                obj.metadata.resource_version = existing.metadata.resource_version;
                api.replace(name, &PostParams::default(), &obj).await?;
            }
            None => {
                api.create(&PostParams::default(), &obj).await?;
            }
        }
        Ok(())
    }

    /// Read a custom resource's `status` object (None if the CR is absent or has no status yet).
    pub async fn get_cr_status(
        &self,
        group: &str,
        version: &str,
        kind: &str,
        ns: &str,
        name: &str,
    ) -> Result<Option<serde_json::Value>, K8sError> {
        let api = self.cr_api(group, version, kind, ns);
        Ok(api
            .get_opt(name)
            .await?
            .and_then(|o| o.data.get("status").cloned()))
    }

    /// Read a custom resource's `spec` object (None if the CR is absent). Sibling of
    /// `get_cr_status`.
    pub async fn get_cr_spec(
        &self,
        group: &str,
        version: &str,
        kind: &str,
        ns: &str,
        name: &str,
    ) -> Result<Option<serde_json::Value>, K8sError> {
        let api = self.cr_api(group, version, kind, ns);
        Ok(api
            .get_opt(name)
            .await?
            .and_then(|o| o.data.get("spec").cloned()))
    }

    /// JSON Merge Patch (RFC 7396) against an arbitrary custom resource — scoped to whatever keys
    /// `merge_patch` contains, unlike `apply_cr`'s full-object replace. Callers should pass the
    /// smallest possible patch (e.g. `{"spec":{"storage":{"nodes":[...]}}}`) so fields the patch
    /// doesn't mention (other top-level `spec` keys) are left untouched.
    pub async fn patch_cr_merge(
        &self,
        group: &str,
        version: &str,
        kind: &str,
        ns: &str,
        name: &str,
        merge_patch: serde_json::Value,
    ) -> Result<(), K8sError> {
        let api = self.cr_api(group, version, kind, ns);
        api.patch(name, &PatchParams::default(), &Patch::Merge(&merge_patch))
            .await?;
        Ok(())
    }

    /// Minimal Kubernetes Node lookup — nothing else in this crate enumerates cluster Nodes today.
    /// Used to validate an operator-supplied node name is real before patching a CR against it.
    pub async fn get_node(
        &self,
        name: &str,
    ) -> Result<Option<k8s_openapi::api::core::v1::Node>, K8sError> {
        let api: Api<k8s_openapi::api::core::v1::Node> = Api::all(self.client.clone());
        Ok(api.get_opt(name).await?)
    }

    /// Delete a custom resource (ignores not-found).
    pub async fn delete_cr(
        &self,
        group: &str,
        version: &str,
        kind: &str,
        ns: &str,
        name: &str,
    ) -> Result<(), K8sError> {
        let api = self.cr_api(group, version, kind, ns);
        match api.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// List every custom resource of a given `group/version/kind` in a namespace, following
    /// pagination. Used where `get_cr_status` (single, by-name) isn't enough — e.g. enumerating
    /// every Rook `CephBlockPool`/`CephFilesystem`/`CephObjectStore` CR in a namespace.
    pub async fn list_crs(
        &self,
        group: &str,
        version: &str,
        kind: &str,
        ns: &str,
    ) -> Result<Vec<DynamicObject>, K8sError> {
        let api = self.cr_api(group, version, kind, ns);
        Ok(list_all(&api, 500).await?)
    }

    // ---- object storage (RGW via ObjectBucketClaim) ----

    fn obc_api(&self, ns: &str) -> (Api<DynamicObject>, ApiResource) {
        let gvk = GroupVersionKind::gvk("objectbucket.io", "v1alpha1", "ObjectBucketClaim");
        let ar = ApiResource::from_gvk(&gvk);
        (Api::namespaced_with(self.client.clone(), ns, &ar), ar)
    }

    /// Create an ObjectBucketClaim; Rook provisions the bucket + a Secret + ConfigMap (same name).
    /// `additional_config` becomes the OBC `spec.additionalConfig` (e.g. `maxObjects`, `maxSize`).
    pub async fn create_obc(
        &self,
        ns: &str,
        name: &str,
        storage_class: &str,
        additional_config: &BTreeMap<String, String>,
    ) -> Result<(), K8sError> {
        let (api, ar) = self.obc_api(ns);
        let mut obj = DynamicObject::new(name, &ar);
        obj.metadata.namespace = Some(ns.to_string());
        let mut spec = serde_json::json!({
            "generateBucketName": name, "storageClassName": storage_class
        });
        if !additional_config.is_empty() {
            spec["additionalConfig"] =
                serde_json::to_value(additional_config).unwrap_or_else(|_| serde_json::json!({}));
        }
        obj.data = serde_json::json!({ "spec": spec });
        api.create(&PostParams::default(), &obj).await?;
        Ok(())
    }

    /// Whether an OBC reports `status.phase == Bound`.
    pub async fn obc_bound(&self, ns: &str, name: &str) -> Result<bool, K8sError> {
        let (api, _ar) = self.obc_api(ns);
        Ok(api
            .get_opt(name)
            .await?
            .and_then(|o| {
                o.data
                    .get("status")
                    .and_then(|s| s.get("phase"))
                    .and_then(|p| p.as_str())
                    .map(|p| p.eq_ignore_ascii_case("bound"))
            })
            .unwrap_or(false))
    }

    /// Delete an OBC (releases the bucket per its reclaim policy).
    pub async fn delete_obc(&self, ns: &str, name: &str) -> Result<(), K8sError> {
        let (api, _ar) = self.obc_api(ns);
        api.delete(name, &DeleteParams::default()).await?;
        Ok(())
    }

    /// Read a ConfigMap's `data` (string values). Used for OBC bucket/endpoint outputs.
    pub async fn get_configmap(
        &self,
        ns: &str,
        name: &str,
    ) -> Result<Option<BTreeMap<String, String>>, K8sError> {
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), ns);
        Ok(api
            .get_opt(name)
            .await?
            .and_then(|c| c.data)
            .map(|d| d.into_iter().collect()))
    }

    /// Resolve a PVC to its backing Ceph RBD `(pool, image)` via the bound PV's CSI attributes.
    /// Returns None if the PVC/PV isn't (yet) a ceph-csi RBD volume.
    pub async fn resolve_rbd(
        &self,
        pvc_ns: &str,
        pvc_name: &str,
    ) -> Result<Option<(String, String)>, K8sError> {
        let Some(pvc) = self.pvc_api(pvc_ns).get_opt(pvc_name).await? else {
            return Ok(None);
        };
        let Some(vol_name) = pvc.spec.and_then(|s| s.volume_name) else {
            return Ok(None);
        };
        let pv_api: Api<PersistentVolume> = Api::all(self.client.clone());
        let Some(pv) = pv_api.get_opt(&vol_name).await? else {
            return Ok(None);
        };
        let attrs = pv
            .spec
            .and_then(|s| s.csi)
            .and_then(|c| c.volume_attributes)
            .unwrap_or_default();
        match (attrs.get("pool"), attrs.get("imageName")) {
            (Some(pool), Some(image)) => Ok(Some((pool.clone(), image.clone()))),
            _ => Ok(None),
        }
    }

    /// Reverse of `resolve_rbd`: map every Rook/Ceph CSI **RBD image name** → the PVC that owns
    /// it (namespace, PVC name, storage class), derived from cluster PersistentVolumes. Discovery
    /// uses this to attribute raw RBD images (which `rbd ls` alone can't tie to Kubernetes) back to
    /// their VM disk. Best-effort — PVs without a CSI `imageName` or a bound `claimRef` are skipped.
    pub async fn rbd_image_owners(
        &self,
    ) -> Result<std::collections::HashMap<String, RbdImageOwner>, K8sError> {
        let api: Api<PersistentVolume> = Api::all(self.client.clone());
        let items = list_all(&api, 1000).await?;
        let mut out = std::collections::HashMap::new();
        for pv in items {
            let Some(spec) = pv.spec else { continue };
            let storage_class = spec.storage_class_name.clone();
            let image = spec
                .csi
                .as_ref()
                .and_then(|c| c.volume_attributes.as_ref())
                .and_then(|a| a.get("imageName").cloned());
            let (Some(image), Some(claim)) = (image, spec.claim_ref) else {
                continue;
            };
            if let (Some(namespace), Some(pvc_name)) = (claim.namespace, claim.name) {
                out.insert(
                    image,
                    RbdImageOwner {
                        namespace,
                        pvc_name,
                        storage_class,
                    },
                );
            }
        }
        Ok(out)
    }

    /// Read a Secret's `data`, base64-decoded to strings. Credentials stay in this process; the
    /// caller must not log them.
    pub async fn get_secret(
        &self,
        ns: &str,
        name: &str,
    ) -> Result<Option<BTreeMap<String, String>>, K8sError> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), ns);
        match api.get_opt(name).await? {
            Some(s) => {
                let data = s
                    .data
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(k, v)| (k, String::from_utf8_lossy(&v.0).to_string()))
                    .collect::<BTreeMap<String, String>>();
                Ok(Some(data))
            }
            None => Ok(None),
        }
    }
}

/// Compact PVC view for the inventory / API.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PvcSummary {
    pub name: String,
    pub namespace: String,
    pub phase: Option<String>,
    pub storage_class: Option<String>,
    pub volume_name: Option<String>,
    pub requested: Option<String>,
    pub access_modes: Vec<String>,
    pub volume_mode: Option<String>,
}

impl From<PersistentVolumeClaim> for PvcSummary {
    fn from(p: PersistentVolumeClaim) -> Self {
        let spec = p.spec.unwrap_or_default();
        let requested = spec
            .resources
            .and_then(|r| r.requests)
            .and_then(|m| m.get("storage").map(|q| q.0.clone()));
        Self {
            name: p.metadata.name.unwrap_or_default(),
            namespace: p.metadata.namespace.unwrap_or_default(),
            phase: p.status.and_then(|s| s.phase),
            storage_class: spec.storage_class_name,
            volume_name: spec.volume_name,
            requested,
            access_modes: spec.access_modes.unwrap_or_default(),
            volume_mode: spec.volume_mode,
        }
    }
}

/// Owning PVC of a Ceph RBD image, resolved from a PersistentVolume's `claimRef`.
#[derive(Debug, Clone)]
pub struct RbdImageOwner {
    pub namespace: String,
    pub pvc_name: String,
    pub storage_class: Option<String>,
}

/// Compact PV view for the inventory / API.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PvSummary {
    pub name: String,
    pub phase: Option<String>,
    pub storage_class: Option<String>,
    pub capacity: Option<String>,
    pub reclaim_policy: Option<String>,
    pub csi_driver: Option<String>,
}

impl From<PersistentVolume> for PvSummary {
    fn from(p: PersistentVolume) -> Self {
        let spec = p.spec.unwrap_or_default();
        let capacity = spec
            .capacity
            .and_then(|m| m.get("storage").map(|q| q.0.clone()));
        let csi_driver = spec.csi.map(|c| c.driver);
        Self {
            name: p.metadata.name.unwrap_or_default(),
            phase: p.status.and_then(|s| s.phase),
            storage_class: spec.storage_class_name,
            capacity,
            reclaim_policy: spec.persistent_volume_reclaim_policy,
            csi_driver,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceph_provisioner_detection() {
        assert!(is_ceph_provisioner("rook-ceph.rbd.csi.ceph.com"));
        assert!(is_ceph_provisioner("rbd.csi.ceph.com"));
        assert!(is_ceph_provisioner("cephfs.csi.ceph.com"));
        assert!(!is_ceph_provisioner("nfs.csi.k8s.io"));
        assert!(!is_ceph_provisioner("kubernetes.io/no-provisioner"));
    }
}
