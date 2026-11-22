// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Kubernetes plumbing for turning a formatted, host-mounted disk into a usable volume: a
//! `no-provisioner` StorageClass, a `local` PersistentVolume pinned to the node, a PVC bound to it,
//! and the throwaway node-prep Job (status + logs) that formats and mounts the disk on the host.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, VolumeResourceRequirements};
use k8s_openapi::api::storage::v1::StorageClass;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::Api;

use crate::{K8sDriver, K8sError};

fn pv_api(driver: &K8sDriver) -> Api<DynamicObject> {
    let ar = ApiResource::from_gvk(&GroupVersionKind::gvk("", "v1", "PersistentVolume"));
    Api::all_with(driver.client.clone(), &ar)
}

impl K8sDriver {
    /// Create the `kubernetes.io/no-provisioner` StorageClass local PVs bind through (idempotent).
    pub async fn ensure_local_storage_class(&self, name: &str) -> Result<(), K8sError> {
        let api: Api<StorageClass> = Api::all(self.client.clone());
        if api.get_opt(name).await?.is_some() {
            return Ok(());
        }
        let sc = StorageClass {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            provisioner: "kubernetes.io/no-provisioner".to_string(),
            reclaim_policy: Some("Retain".to_string()),
            volume_binding_mode: Some("WaitForFirstConsumer".to_string()),
            ..Default::default()
        };
        api.create(&PostParams::default(), &sc).await?;
        Ok(())
    }

    /// Create a `local` PV for `host_path` on `node` (no-op when it already exists).
    pub async fn apply_local_pv(
        &self,
        name: &str,
        host_path: &str,
        size_bytes: i64,
        node: &str,
        storage_class: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<(), K8sError> {
        let api = pv_api(self);
        if api.get_opt(name).await?.is_some() {
            return Ok(());
        }
        let ar = ApiResource::from_gvk(&GroupVersionKind::gvk("", "v1", "PersistentVolume"));
        let mut obj = DynamicObject::new(name, &ar);
        obj.metadata.labels = Some(labels.clone());
        obj.data = serde_json::json!({
            "spec": {
                "capacity": { "storage": size_bytes.to_string() },
                "accessModes": ["ReadWriteOnce"],
                "persistentVolumeReclaimPolicy": "Retain",
                "storageClassName": storage_class,
                "volumeMode": "Filesystem",
                "local": { "path": host_path },
                "nodeAffinity": { "required": { "nodeSelectorTerms": [{
                    "matchExpressions": [{
                        "key": "kubernetes.io/hostname",
                        "operator": "In",
                        "values": [node]
                    }]
                }]}}
            }
        });
        api.create(&PostParams::default(), &obj).await?;
        Ok(())
    }

    /// PVs carrying `label_selector` (e.g. `atlas.zyvor.dev/example=true`), as raw JSON so the
    /// caller can show node affinity, path and claim without a typed model.
    pub async fn list_pvs_json(&self, label_selector: &str) -> Result<Vec<serde_json::Value>, K8sError> {
        let list = pv_api(self)
            .list(&ListParams::default().labels(label_selector))
            .await?;
        Ok(list
            .items
            .into_iter()
            .map(|o| serde_json::to_value(o).unwrap_or_default())
            .collect())
    }

    /// Delete a PV (ignores not-found).
    pub async fn delete_pv(&self, name: &str) -> Result<(), K8sError> {
        match pv_api(self).delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// A PVC that binds to the named PV (`volumeName`) through `storage_class`.
    pub async fn create_bound_pvc(
        &self,
        ns: &str,
        name: &str,
        pv_name: &str,
        storage_class: &str,
        size_bytes: i64,
        labels: &BTreeMap<String, String>,
    ) -> Result<(), K8sError> {
        let api: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), ns);
        if api.get_opt(name).await?.is_some() {
            return Ok(());
        }
        let mut requests = BTreeMap::new();
        requests.insert("storage".to_string(), Quantity(size_bytes.to_string()));
        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(ns.to_string()),
                labels: Some(labels.clone()),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                storage_class_name: Some(storage_class.to_string()),
                volume_name: Some(pv_name.to_string()),
                resources: Some(VolumeResourceRequirements {
                    requests: Some(requests),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        api.create(&PostParams::default(), &pvc).await?;
        Ok(())
    }

    /// `Some(true)` once the Job has a succeeded pod, `Some(false)` once it has failed, `None`
    /// while it is still running (or not yet visible).
    pub async fn job_outcome(&self, ns: &str, name: &str) -> Result<Option<bool>, K8sError> {
        let status = self.get_cr_status("batch", "v1", "Job", ns, name).await?;
        Ok(status.and_then(|s| {
            if s.get("succeeded").and_then(|v| v.as_i64()).unwrap_or(0) > 0 {
                Some(true)
            } else if s.get("failed").and_then(|v| v.as_i64()).unwrap_or(0) > 0 {
                Some(false)
            } else {
                None
            }
        }))
    }

    /// The combined log of the Job's pods (the node-prep script echoes each step).
    pub async fn job_logs(&self, ns: &str, job: &str) -> Result<String, K8sError> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        let list = pods
            .list(&ListParams::default().labels(&format!("job-name={job}")))
            .await?;
        let mut out = String::new();
        for p in list.items {
            if let Some(name) = p.metadata.name {
                if let Ok(log) = pods.logs(&name, &LogParams::default()).await {
                    out.push_str(&log);
                }
            }
        }
        Ok(out)
    }

    /// Delete a Job and its pods (background propagation; ignores not-found).
    pub async fn delete_job(&self, ns: &str, name: &str) -> Result<(), K8sError> {
        let ar = ApiResource::from_gvk(&GroupVersionKind::gvk("batch", "v1", "Job"));
        let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), ns, &ar);
        match api.delete(name, &DeleteParams::background()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

impl K8sDriver {
    /// Namespaced objects of any kind as raw JSON (`group` is empty for core kinds), optionally
    /// filtered by a label selector.
    pub async fn list_json(
        &self,
        group: &str,
        version: &str,
        kind: &str,
        ns: &str,
        label_selector: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, K8sError> {
        let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, kind));
        let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), ns, &ar);
        let mut lp = ListParams::default();
        if let Some(sel) = label_selector {
            lp = lp.labels(sel);
        }
        Ok(api
            .list(&lp)
            .await?
            .items
            .into_iter()
            .map(|o| serde_json::to_value(o).unwrap_or_default())
            .collect())
    }

    /// Strategic-merge patch a Deployment (used to repoint the gateway's own environment).
    pub async fn patch_deployment(
        &self,
        ns: &str,
        name: &str,
        patch: serde_json::Value,
    ) -> Result<(), K8sError> {
        let ar = ApiResource::from_gvk(&GroupVersionKind::gvk("apps", "v1", "Deployment"));
        let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), ns, &ar);
        api.patch(name, &PatchParams::default(), &Patch::Strategic(&patch))
            .await?;
        Ok(())
    }
}
