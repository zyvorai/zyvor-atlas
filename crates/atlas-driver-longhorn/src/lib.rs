// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Longhorn through its Kubernetes CRDs (`longhorn.io/v1beta2`). Discovery reads `nodes` and
//! `volumes`. Create, delete and expand post or patch `volumes`; snapshots are `snapshots` CRs.
//! Clone and backup are not implemented. A volume whose native id is a PVC stays on the
//! Kubernetes PVC path — this driver refuses to mutate it through the Volume CR.

use std::collections::BTreeMap;

use async_trait::async_trait;
use atlas_api_types::{
    CreateSnapshotRequest, CreateSnapshotResult, CreateVolumeRequest, CreateVolumeResult,
    DeleteSnapshotRequest, DeleteVolumeRequest, DiscoveryResult, ExpandVolumeRequest, Health,
    MetricSample, StorageCluster, StorageHealth, StoragePool, StorageVolume, VolumeKind,
};
use atlas_driver_core::{DriverError, StorageDriver};
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::{Api, Client};
use serde_json::{json, Value};

pub struct LonghornDriver {
    backend_id: String,
    namespace: String,
    client: Client,
}

impl LonghornDriver {
    pub fn new(backend_id: impl Into<String>, namespace: impl Into<String>, client: Client) -> Self {
        Self { backend_id: backend_id.into(), namespace: namespace.into(), client }
    }

    fn api(&self, kind: &str, plural: &str) -> Api<DynamicObject> {
        Api::namespaced_with(self.client.clone(), &self.namespace, &resource(kind, plural))
    }

    async fn create_cr(
        &self,
        kind: &str,
        plural: &str,
        name: &str,
        spec: Value,
        labels: Option<BTreeMap<String, String>>,
    ) -> Result<String, DriverError> {
        let api = self.api(kind, plural);
        let obj = custom_resource(kind, plural, &self.namespace, name, spec, labels);
        let created = api.create(&PostParams::default(), &obj).await.map_err(|e| {
            kube_err(&format!("create {plural}/{name}"), &self.namespace, e)
        })?;
        Ok(created.metadata.name.clone().unwrap_or_else(|| name.to_string()))
    }

    async fn delete_cr(&self, kind: &str, plural: &str, name: &str) -> Result<(), DriverError> {
        let api = self.api(kind, plural);
        match api.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.is_not_found() => Ok(()),
            Err(e) => Err(kube_err(&format!("delete {plural}/{name}"), &self.namespace, e)),
        }
    }

    async fn list(&self, kind: &str, plural: &str) -> Result<Vec<Value>, DriverError> {
        let api = self.api(kind, plural);
        let mut params = ListParams::default().limit(500);
        let mut result = Vec::new();
        loop {
            let page = api.list(&params).await.map_err(|e| DriverError::Unreachable(format!(
                "Longhorn {plural} in namespace {}: {e}", self.namespace
            )))?;
            result.extend(page.items.into_iter().map(|item| serde_json::to_value(item)
                .map_err(|e| DriverError::Parse(e.to_string()))).collect::<Result<Vec<_>, _>>()?);
            match page.metadata.continue_.as_deref() {
                Some(token) if !token.is_empty() => params = params.continue_token(token),
                _ => return Ok(result),
            }
        }
    }

    async fn snapshot(&self) -> Result<DiscoveryResult, DriverError> {
        let nodes = self.list("Node", "nodes").await?;
        let volumes = self.list("Volume", "volumes").await?;
        Ok(build_discovery(&self.backend_id, &self.namespace, &nodes, &volumes))
    }
}

fn number(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn value_at<'a>(v: &'a Value, path: &str) -> &'a Value {
    v.pointer(path).unwrap_or(&Value::Null)
}

fn name(v: &Value) -> Option<&str> { value_at(v, "/metadata/name").as_str() }

fn resource(kind: &str, plural: &str) -> ApiResource {
    let mut resource = ApiResource::from_gvk(&GroupVersionKind::gvk("longhorn.io", "v1beta2", kind));
    resource.plural = plural.into();
    resource
}

/// The Volume body Longhorn's own examples create. The admission webhook fills the remaining
/// spec when the manager is installed; unknown fields are omitted so an older CRD does not reject
/// the object.
fn volume_spec(size_bytes: i64, access_mode: &str) -> Value {
    json!({
        "size": size_bytes.to_string(),
        "numberOfReplicas": 3,
        "frontend": "blockdev",
        "accessMode": access_mode,
        "dataLocality": "disabled",
        "staleReplicaTimeout": 20,
        "fromBackup": "",
        "dataSource": "",
    })
}

fn snapshot_spec(volume: &str) -> Value {
    json!({ "volume": volume, "createSnapshot": true })
}

fn snapshot_labels(volume: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("longhornvolume".to_string(), volume.to_string())])
}

fn custom_resource(
    kind: &str,
    plural: &str,
    namespace: &str,
    name: &str,
    spec: Value,
    labels: Option<BTreeMap<String, String>>,
) -> DynamicObject {
    let mut obj = DynamicObject::new(name, &resource(kind, plural));
    obj.metadata.namespace = Some(namespace.to_string());
    obj.metadata.labels = labels;
    obj.data = json!({ "spec": spec });
    obj
}

fn requested_access_mode(req: &CreateVolumeRequest) -> &'static str {
    let rwx = req.kubernetes.as_ref().is_some_and(|k| {
        k.access_modes.iter().any(|m| {
            m.eq_ignore_ascii_case("ReadWriteMany") || m.eq_ignore_ascii_case("rwx")
        })
    });
    if rwx { "rwx" } else { "rwo" }
}

fn is_dns1123_subdomain(name: &str) -> bool {
    if name.is_empty() || name.len() > 253 {
        return false;
    }
    name.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && label.starts_with(|c: char| c.is_ascii_alphanumeric())
            && label.ends_with(|c: char| c.is_ascii_alphanumeric())
    })
}

fn require_k8s_name(name: &str) -> Result<(), DriverError> {
    if is_dns1123_subdomain(name) {
        Ok(())
    } else {
        Err(DriverError::Backend(format!(
            "invalid: {name:?} must be a Kubernetes resource name"
        )))
    }
}

fn require_positive(size: i64) -> Result<(), DriverError> {
    if size > 0 {
        Ok(())
    } else {
        Err(DriverError::Backend("invalid: size_bytes must be > 0".into()))
    }
}

/// A shrink is refused before the API call. A missing current size is left for Longhorn to judge.
fn require_growth(current: Option<i64>, new_size: i64) -> Result<(), DriverError> {
    require_positive(new_size)?;
    if let Some(current) = current {
        if new_size <= current {
            return Err(DriverError::Backend(format!(
                "invalid: new size {new_size} is not larger than {current}"
            )));
        }
    }
    Ok(())
}

/// Longhorn volume name from an Atlas id, a `longhorn/{namespace}/{name}` native id, or the CR name.
/// `pvc/...` belongs to the Kubernetes PVC path.
fn volume_name(backend_id: &str, namespace: &str, id: &str) -> Result<String, DriverError> {
    let id = id.trim();
    if id.is_empty() {
        return Err(DriverError::Backend("invalid: volume id is empty".into()));
    }
    if id.starts_with("pvc/") {
        return Err(DriverError::Backend(format!(
            "invalid: {id} is a Kubernetes PVC; change it through the PVC, not the Longhorn volume CR"
        )));
    }
    if let Some(rest) = id.strip_prefix("longhorn/") {
        let mut parts = rest.split('/');
        return match (parts.next(), parts.next(), parts.next()) {
            (Some(ns), Some(name), None) if !ns.is_empty() && !name.is_empty() => {
                if ns != namespace {
                    return Err(DriverError::Backend(format!(
                        "invalid: volume {id} is in namespace {ns}; this driver manages {namespace}"
                    )));
                }
                require_k8s_name(name)?;
                Ok(name.to_string())
            }
            _ => Err(DriverError::Backend(format!(
                "invalid: volume id {id} is not longhorn/{{namespace}}/{{name}}"
            ))),
        };
    }
    let prefix = format!("vol_{backend_id}_");
    let name = id.strip_prefix(&prefix).unwrap_or(id);
    require_k8s_name(name)?;
    Ok(name.to_string())
}

fn snapshot_name(backend_id: &str, namespace: &str, id: &str) -> Result<String, DriverError> {
    let id = id.trim();
    if id.is_empty() {
        return Err(DriverError::Backend("invalid: snapshot id is empty".into()));
    }
    if let Some(rest) = id.strip_prefix("longhorn-snap/") {
        let mut parts = rest.split('/');
        return match (parts.next(), parts.next(), parts.next()) {
            (Some(ns), Some(name), None) if !ns.is_empty() && !name.is_empty() => {
                if ns != namespace {
                    return Err(DriverError::Backend(format!(
                        "invalid: snapshot {id} is in namespace {ns}; this driver manages {namespace}"
                    )));
                }
                require_k8s_name(name)?;
                Ok(name.to_string())
            }
            _ => Err(DriverError::Backend(format!(
                "invalid: snapshot id {id} is not longhorn-snap/{{namespace}}/{{name}}"
            ))),
        };
    }
    let prefix = format!("snap_{backend_id}_");
    let name = id.strip_prefix(&prefix).unwrap_or(id);
    require_k8s_name(name)?;
    Ok(name.to_string())
}

fn kube_err(op: &str, namespace: &str, err: kube::Error) -> DriverError {
    match err {
        kube::Error::Api(e) => {
            DriverError::Backend(format!("Longhorn {op} in namespace {namespace}: {e}"))
        }
        other => DriverError::Unreachable(format!("Longhorn {op} in namespace {namespace}: {other}")),
    }
}

fn build_discovery(backend_id: &str, namespace: &str, nodes: &[Value], volumes: &[Value]) -> DiscoveryResult {
    let cluster_id = format!("cls_{backend_id}");
    let pool_id = format!("pool_{backend_id}");
    // Longhorn disk status reports physical storageMaximum/storageAvailable; these are not
    // equivalent to the sum of provisioned logical volume sizes.
    let mut total: i64 = 0;
    let mut available: i64 = 0;
    let mut disks_seen = 0;
    let mut unhealthy = false;
    let mut faulted = false;
    for node in nodes {
        let ready = value_at(node, "/status/conditions").as_object()
            .and_then(|c| c.get("Ready"))
            .and_then(|c| c.get("status"))
            .and_then(Value::as_str);
        if ready != Some("True") { unhealthy = true; }
        if let Some(disks) = value_at(node, "/status/diskStatus").as_object() {
            for disk in disks.values() {
                if let (Some(max), Some(free)) = (
                    number(value_at(disk, "/storageMaximum")),
                    number(value_at(disk, "/storageAvailable")),
                ) {
                    total = total.saturating_add(max);
                    available = available.saturating_add(free);
                    disks_seen += 1;
                }
            }
        }
    }
    let capacity = (disks_seen > 0).then_some(total);
    let free = (disks_seen > 0).then_some(available);
    let used = (disks_seen > 0).then_some(total.saturating_sub(available));
    let mut mapped = Vec::new();
    for v in volumes {
        let Some(volume_name) = name(v) else { continue };
        let state = value_at(v, "/status/state").as_str().unwrap_or("unknown");
        let robustness = value_at(v, "/status/robustness").as_str().unwrap_or("unknown");
        let health = match robustness {
            "healthy" => Health::Ok,
            "degraded" => { unhealthy = true; Health::Warn },
            "faulted" => { unhealthy = true; faulted = true; Health::Critical },
            _ => Health::Unknown,
        };
        mapped.push(StorageVolume {
            id: format!("vol_{backend_id}_{volume_name}"),
            cluster_id: Some(cluster_id.clone()), pool_id: Some(pool_id.clone()),
            name: volume_name.into(), kind: VolumeKind::Block,
            backend_native_id: match (
                value_at(v, "/status/kubernetesStatus/namespace").as_str(),
                value_at(v, "/status/kubernetesStatus/pvcName").as_str(),
            ) {
                (Some(ns), Some(pvc)) if !ns.is_empty() && !pvc.is_empty() => Some(format!("pvc/{ns}/{pvc}")),
                _ => Some(format!("longhorn/{namespace}/{volume_name}")),
            },
            size_bytes: number(value_at(v, "/spec/size")).unwrap_or(0),
            used_bytes: number(value_at(v, "/status/actualSize")),
            state: state.into(), health,
            kubernetes_namespace: value_at(v, "/status/kubernetesStatus/namespace").as_str().map(str::to_owned),
            pvc_name: value_at(v, "/status/kubernetesStatus/pvcName").as_str().map(str::to_owned),
            storage_class_name: value_at(v, "/status/kubernetesStatus/storageClassName").as_str().map(str::to_owned),
        });
    }
    let status = if faulted { Health::Critical } else if nodes.is_empty() {
        Health::Unknown
    } else if unhealthy { Health::Warn } else { Health::Ok };
    let health = StorageHealth {
        status, summary: format!("Longhorn: {} node(s), {} volume(s)", nodes.len(), mapped.len()),
        raw_capacity_bytes: capacity, used_capacity_bytes: used, available_capacity_bytes: free,
        recovering: volumes.iter().any(|v| value_at(v, "/status/robustness") == "degraded"),
        degraded_objects: mapped.iter().filter(|v| matches!(v.health, Health::Warn | Health::Critical)).count() as i64,
    };
    DiscoveryResult {
        cluster: StorageCluster {
            id: cluster_id.clone(), backend_id: backend_id.into(),
            name: format!("longhorn/{namespace}"), native_fsid: None,
            health: status, raw_capacity_bytes: capacity,
            used_capacity_bytes: used, available_capacity_bytes: free,
        },
        pools: vec![StoragePool {
            id: pool_id, cluster_id, name: "longhorn".into(), kind: "longhorn".into(),
            device_class: None, replica_size: None, used_bytes: used, max_bytes: capacity, health: status,
        }], osds: vec![], volumes: mapped, health,
    }
}

#[async_trait]
impl StorageDriver for LonghornDriver {
    fn backend_id(&self) -> &str { &self.backend_id }
    async fn discover(&self) -> Result<DiscoveryResult, DriverError> { self.snapshot().await }
    async fn health(&self) -> Result<StorageHealth, DriverError> { Ok(self.snapshot().await?.health) }
    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> { Ok(self.snapshot().await?.pools) }
    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        if pool != "longhorn" { return Ok(vec![]); }
        Ok(self.snapshot().await?.volumes)
    }
    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        let snapshot = self.snapshot().await?;
        let sample = |name: &str, value: f64| MetricSample {
            name: name.into(), value, labels: Default::default()
        };
        let mut result = vec![sample("longhorn_volumes_total", snapshot.volumes.len() as f64)];
        if let Some(capacity) = snapshot.cluster.raw_capacity_bytes {
            result.push(sample("longhorn_capacity_bytes", capacity as f64));
        }
        Ok(result)
    }

    async fn create_volume(&self, req: CreateVolumeRequest) -> Result<CreateVolumeResult, DriverError> {
        if req.kind != VolumeKind::Block {
            return Err(DriverError::Backend(
                "invalid: Longhorn native volumes are block devices".into(),
            ));
        }
        let name = req.name.trim();
        require_k8s_name(name)?;
        require_positive(req.size_bytes)?;
        let access = requested_access_mode(&req);
        let created = self
            .create_cr("Volume", "volumes", name, volume_spec(req.size_bytes, access), None)
            .await?;
        let backend_id = &self.backend_id;
        let namespace = &self.namespace;
        Ok(CreateVolumeResult {
            volume_id: format!("vol_{backend_id}_{created}"),
            backend_native_id: format!("longhorn/{namespace}/{created}"),
        })
    }

    async fn expand_volume(&self, req: ExpandVolumeRequest) -> Result<(), DriverError> {
        let name = volume_name(&self.backend_id, &self.namespace, &req.volume_id)?;
        let api = self.api("Volume", "volumes");
        let current = api.get(&name).await.map_err(|e| {
            kube_err(&format!("get volumes/{name}"), &self.namespace, e)
        })?;
        let current_size = current.data.pointer("/spec/size").and_then(number);
        require_growth(current_size, req.new_size_bytes)?;
        let patch = json!({ "spec": { "size": req.new_size_bytes.to_string() } });
        api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .map_err(|e| kube_err(&format!("expand volumes/{name}"), &self.namespace, e))?;
        Ok(())
    }

    async fn delete_volume(&self, req: DeleteVolumeRequest) -> Result<(), DriverError> {
        let name = volume_name(&self.backend_id, &self.namespace, &req.volume_id)?;
        self.delete_cr("Volume", "volumes", &name).await
    }

    async fn create_snapshot(
        &self,
        req: CreateSnapshotRequest,
    ) -> Result<CreateSnapshotResult, DriverError> {
        let volume = volume_name(&self.backend_id, &self.namespace, &req.volume_id)?;
        let name = req.name.trim();
        require_k8s_name(name)?;
        let created = self
            .create_cr(
                "Snapshot",
                "snapshots",
                name,
                snapshot_spec(&volume),
                Some(snapshot_labels(&volume)),
            )
            .await?;
        let backend_id = &self.backend_id;
        let namespace = &self.namespace;
        Ok(CreateSnapshotResult {
            snapshot_id: format!("snap_{backend_id}_{created}"),
            backend_native_id: format!("longhorn-snap/{namespace}/{created}"),
        })
    }

    async fn delete_snapshot(&self, req: DeleteSnapshotRequest) -> Result<(), DriverError> {
        let name = snapshot_name(&self.backend_id, &self.namespace, &req.snapshot_id)?;
        self.delete_cr("Snapshot", "snapshots", &name).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_capacity_and_volume_health_without_inventing_missing_values() {
        let nodes = [json!({"metadata":{"name":"node-1"},"status":{
            "conditions":{"Ready":{"status":"True"}},
            "diskStatus":{"disk1":{"storageMaximum":1000,"storageAvailable":600}}
        }})];
        let volumes = [json!({"metadata":{"name":"pvc-a"},"spec":{"size":"400"},
            "status":{"state":"attached","robustness":"degraded","actualSize":120,
            "kubernetesStatus":{"namespace":"vms","pvcName":"disk-a","storageClassName":"longhorn"}}
        })];
        let result = build_discovery("bkd_longhorn", "longhorn-system", &nodes, &volumes);
        assert_eq!(result.cluster.raw_capacity_bytes, Some(1000));
        assert_eq!(result.cluster.available_capacity_bytes, Some(600));
        assert_eq!(result.health.status, Health::Warn);
        assert_eq!(result.volumes[0].pvc_name.as_deref(), Some("disk-a"));
        assert_eq!(result.volumes[0].size_bytes, 400);
        assert_eq!(result.volumes[0].health, Health::Warn);
        assert_eq!(result.volumes[0].backend_native_id.as_deref(), Some("pvc/vms/disk-a"));
        let unknown = build_discovery("bkd_longhorn", "longhorn-system", &[], &[]);
        assert_eq!(unknown.cluster.raw_capacity_bytes, None);
        assert_eq!(unknown.health.status, Health::Unknown);
    }

    #[test]
    fn volume_and_snapshot_bodies_match_the_longhorn_crd() {
        let spec = volume_spec(10_737_418_240, "rwo");
        assert_eq!(spec["size"], "10737418240");
        assert_eq!(spec["numberOfReplicas"], 3);
        assert_eq!(spec["frontend"], "blockdev");
        assert_eq!(spec["accessMode"], "rwo");
        let vol = custom_resource("Volume", "volumes", "longhorn-system", "disk-a", spec, None);
        let v = serde_json::to_value(&vol).unwrap();
        assert_eq!(v["apiVersion"], "longhorn.io/v1beta2");
        assert_eq!(v["kind"], "Volume");
        assert_eq!(v["metadata"]["namespace"], "longhorn-system");
        assert_eq!(v["spec"]["size"], "10737418240");

        let snap = custom_resource(
            "Snapshot",
            "snapshots",
            "longhorn-system",
            "disk-a-snap",
            snapshot_spec("disk-a"),
            Some(snapshot_labels("disk-a")),
        );
        let s = serde_json::to_value(&snap).unwrap();
        assert_eq!(s["kind"], "Snapshot");
        assert_eq!(s["spec"]["volume"], "disk-a");
        assert_eq!(s["spec"]["createSnapshot"], true);
        assert_eq!(s["metadata"]["labels"]["longhornvolume"], "disk-a");
    }

    #[test]
    fn parses_native_ids_and_refuses_pvc_shrink_and_bad_names() {
        assert_eq!(
            volume_name("bkd_longhorn", "longhorn-system", "longhorn/longhorn-system/disk-a").unwrap(),
            "disk-a"
        );
        assert_eq!(volume_name("bkd_longhorn", "longhorn-system", "vol_bkd_longhorn_disk-a").unwrap(), "disk-a");
        assert_eq!(volume_name("bkd_longhorn", "longhorn-system", "disk-a").unwrap(), "disk-a");
        assert!(volume_name("bkd_longhorn", "longhorn-system", "pvc/vms/disk-a").is_err());
        assert!(volume_name("bkd_longhorn", "longhorn-system", "longhorn/other/disk-a").is_err());
        assert!(volume_name("bkd_longhorn", "longhorn-system", "Disk_A").is_err());

        assert_eq!(
            snapshot_name("bkd_longhorn", "longhorn-system", "longhorn-snap/longhorn-system/disk-a-snap").unwrap(),
            "disk-a-snap"
        );
        assert_eq!(
            snapshot_name("bkd_longhorn", "longhorn-system", "snap_bkd_longhorn_disk-a-snap").unwrap(),
            "disk-a-snap"
        );
        assert!(snapshot_name("bkd_longhorn", "longhorn-system", "longhorn-snap/other/disk-a-snap").is_err());

        assert!(require_growth(Some(100), 100).is_err());
        assert!(require_growth(Some(100), 50).is_err());
        assert!(require_growth(Some(100), 0).is_err());
        assert!(require_growth(Some(100), 101).is_ok());
        assert!(require_growth(None, 10).is_ok());

        let mut req = CreateVolumeRequest {
            tenant_id: "t".into(),
            name: "disk-a".into(),
            size_bytes: 10,
            kind: VolumeKind::Block,
            policy: None,
            pool: None,
            owner: None,
            kubernetes: None,
        };
        assert_eq!(requested_access_mode(&req), "rwo");
        req.kubernetes = Some(atlas_api_types::K8sVolumeOpts {
            backend_id: None,
            namespace: None,
            create_pvc: true,
            access_modes: vec!["ReadWriteMany".into()],
            volume_mode: None,
            storage_class: None,
        });
        assert_eq!(requested_access_mode(&req), "rwx");
    }
}
