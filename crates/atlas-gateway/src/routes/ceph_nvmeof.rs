// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `GET /ceph/nvmeof`: the Rook `CephNVMeOFGateway` CRs, their pods, and the StorageClasses of the
//! Ceph-CSI NVMe-oF driver. Read-only.
//!
//! Rook (v1.19+, experimental, Ceph Tentacle or later) runs the gateways from the CR; an NVMe
//! namespace is created by a PVC from an `nvmeof.csi.ceph.com` StorageClass, not by the gateway
//! CR, so this view lists the classes rather than any per-namespace state.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{ids, AppError, AppResult};
use atlas_driver_k8s::rook::PodBrief;
use atlas_jobs::JobSpec;

use super::util::{accepted, validate_k8s_name, CEPH_BACKEND_ID};
use crate::auth::Actor;
use crate::state::AppState;

/// Label Rook puts on every NVMe-oF gateway pod.
const GATEWAY_POD_SELECTOR: &str = "app=rook-ceph-nvmeof";

#[derive(Debug, Default, Deserialize)]
pub(crate) struct NvmeofParams {
    namespace: Option<String>,
}

/// One gateway CR plus the state of its pods. Rook names a gateway's pods
/// `rook-ceph-nvmeof-<cr name>-<instance letter>-<hash>`.
pub(crate) fn summarize_gateway(cr: &Value, pods: &[PodBrief]) -> Value {
    let name = cr["metadata"]["name"].as_str().unwrap_or_default();
    let spec = &cr["spec"];
    let instances = spec["instances"].as_u64().unwrap_or(1);
    let prefix = format!("rook-ceph-nvmeof-{name}-");
    let mine: Vec<&PodBrief> = pods.iter().filter(|p| p.name.starts_with(&prefix)).collect();
    let ready = mine.iter().filter(|p| p.ready).count() as u64;

    let mut findings = Vec::new();
    if instances < 2 {
        findings.push("single gateway: no high availability (Ceph raises NVMEOF_SINGLE_GATEWAY)");
    }
    if ready < instances {
        findings.push("fewer gateway pods are ready than the CR asks for");
    }
    json!({
        "name": name,
        "pool": spec["pool"],
        "group": spec["group"],
        "image": spec["image"],
        "instances": instances,
        "host_network": spec["hostNetwork"].as_bool().unwrap_or(false),
        "pods_ready": ready,
        "pods": mine,
        "status": cr["status"],
        "findings": findings,
    })
}

/// `GET /ceph/nvmeof` — gateways, their pods, and the NVMe-oF StorageClasses. `installed` is
/// false when the cluster has no `CephNVMeOFGateway` CRD (Rook older than v1.19).
pub(crate) async fn get_ceph_nvmeof(
    State(s): State<AppState>,
    Query(q): Query<NvmeofParams>,
) -> AppResult<Json<Value>> {
    let k8s = s.k8s.as_ref().ok_or_else(|| {
        AppError::Unavailable("no kubernetes cluster attached; cannot read Rook resources".into())
    })?;
    let ns = q.namespace.unwrap_or_else(|| s.config.rook_namespace.clone());
    let driver_err = |e: atlas_driver_k8s::K8sError| AppError::Driver(e.to_string());

    let Some(crs) = k8s.list_nvmeof_gateways(&ns).await.map_err(driver_err)? else {
        return Ok(Json(json!({
            "installed": false,
            "gateways": [],
            "storage_classes": [],
        })));
    };
    let pods = k8s
        .list_pods_by_label(&ns, GATEWAY_POD_SELECTOR)
        .await
        .map_err(driver_err)?;
    let classes = k8s.list_nvmeof_storage_classes().await.map_err(driver_err)?;
    let gateways: Vec<Value> = crs.iter().map(|cr| summarize_gateway(cr, &pods)).collect();
    Ok(Json(json!({
        "installed": true,
        "namespace": ns,
        "gateways": gateways,
        "storage_classes": classes,
    })))
}

/// Largest namespace one request may ask for (64 TiB).
const MAX_NAMESPACE_BYTES: i64 = 64 << 40;

#[derive(Debug, Deserialize)]
pub(crate) struct CreateNamespaceBody {
    pub tenant_id: String,
    /// Name of the PVC; the namespace is whatever the CSI driver derives from it.
    pub name: String,
    /// Kubernetes namespace of the PVC; defaults to `default`.
    #[serde(default)]
    pub namespace: Option<String>,
    /// An `nvmeof.csi.ceph.com` StorageClass, as listed by `GET /ceph/nvmeof`.
    pub storage_class: String,
    pub size_bytes: i64,
    #[serde(default)]
    pub confirm: bool,
}

/// Request checks that need no cluster access.
pub(crate) fn validate_namespace_request(b: &CreateNamespaceBody) -> Result<(), String> {
    if !b.confirm {
        return Err("confirm=true is required to provision an NVMe-oF namespace".into());
    }
    if b.size_bytes <= 0 || b.size_bytes > MAX_NAMESPACE_BYTES {
        return Err(format!("size_bytes must be between 1 and {MAX_NAMESPACE_BYTES}"));
    }
    if b.storage_class.trim().is_empty() {
        return Err("storage_class is required".into());
    }
    Ok(())
}

/// `POST /ceph/nvmeof/namespaces` — provision a new NVMe-oF namespace by creating a PVC from an
/// NVMe-oF StorageClass (admin, `confirm`). Rook offers no way to export an existing RBD volume,
/// so this only ever creates new ones. Runs as the ordinary `volume.create` job in block mode.
pub(crate) async fn create_nvmeof_namespace(
    State(s): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(body): Json<CreateNamespaceBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::auth::require_role(s.config.auth_required, &actor, crate::auth::ROLE_ADMIN)?;
    validate_namespace_request(&body).map_err(AppError::Validation)?;
    validate_k8s_name(&body.name)?;
    let k8s = s.k8s.as_ref().ok_or_else(|| {
        AppError::Unavailable("no kubernetes cluster attached; cannot provision".into())
    })?;
    let classes = k8s
        .list_nvmeof_storage_classes()
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    if !classes.iter().any(|c| c.name == body.storage_class) {
        return Err(AppError::Validation(format!(
            "storage class {} is not an nvmeof.csi.ceph.com class (see GET /ceph/nvmeof)",
            body.storage_class
        )));
    }
    if atlas_inventory::is_backend_cordoned(&s.pool, CEPH_BACKEND_ID).await? {
        return Err(AppError::Unavailable(format!(
            "backend {CEPH_BACKEND_ID} is cordoned for maintenance"
        )));
    }
    match atlas_inventory::tenants::check_admission(&s.pool, &body.tenant_id, body.size_bytes)
        .await?
    {
        atlas_inventory::tenants::QuotaCheck::Ok => {}
        atlas_inventory::tenants::QuotaCheck::Bytes { limit, would_be } => {
            return Err(AppError::Conflict(format!(
                "tenant {} byte quota exceeded: {would_be} > {limit}",
                body.tenant_id
            )));
        }
        atlas_inventory::tenants::QuotaCheck::Count { limit, current } => {
            return Err(AppError::Conflict(format!(
                "tenant {} volume-count quota exceeded: {current} already at limit {limit}",
                body.tenant_id
            )));
        }
    }

    let namespace = body.namespace.clone().unwrap_or_else(|| "default".into());
    validate_k8s_name(&namespace)?;
    let volume_id = ids::volume_id();
    let job_id = ids::job_id();
    let idem = ids::stable_id(
        "idem",
        &format!(
            "{}|{}|{}|{}|{}|nvmeof.namespace",
            body.tenant_id, namespace, body.name, body.size_bytes, body.storage_class
        ),
    );
    let spec = JobSpec::VolumeCreate {
        volume_id: volume_id.clone(),
        backend_id: CEPH_BACKEND_ID.into(),
        name: body.name.clone(),
        namespace: namespace.clone(),
        storage_class: body.storage_class.clone(),
        access_mode: "ReadWriteOnce".into(),
        volume_mode: "Block".into(),
        size_bytes: body.size_bytes,
        kind: "block".into(),
        policy: None,
        owner: None,
    };
    let job = s
        .jobs
        .enqueue(&job_id, &body.tenant_id, &actor.id, spec, Some(&idem))
        .await
        .map_err(AppError::from)?;
    let _ = atlas_inventory::audit::record(
        &s.pool,
        Some(&body.tenant_id),
        &actor.id,
        "nvmeof.namespace.requested",
        "volume",
        &volume_id,
        "accepted",
        Some(json!({ "name": body.name, "namespace": namespace,
                     "storage_class": body.storage_class, "size_bytes": body.size_bytes })),
        None,
    )
    .await;
    Ok(accepted(
        &job,
        json!({ "volume_id": volume_id, "storage_class": body.storage_class,
                "namespace": namespace, "pvc": body.name }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(name: &str, ready: bool) -> PodBrief {
        PodBrief { name: name.into(), phase: Some("Running".into()), ready, node: Some("n1".into()) }
    }

    fn cr(name: &str, instances: u64) -> Value {
        json!({
            "metadata": { "name": name },
            "spec": { "pool": "nvmeof", "group": "group-a", "image": "quay.io/ceph/nvmeof:1.5",
                      "instances": instances, "hostNetwork": false },
        })
    }

    #[test]
    fn a_single_ready_gateway_is_flagged_as_not_highly_available() {
        let pods = [pod("rook-ceph-nvmeof-nvmeof-a-85844ff6b8-4r8gj", true)];
        let g = summarize_gateway(&cr("nvmeof", 1), &pods);
        assert_eq!(g["instances"], 1);
        assert_eq!(g["pods_ready"], 1);
        assert_eq!(g["findings"].as_array().unwrap().len(), 1);
        assert!(g["findings"][0].as_str().unwrap().contains("single gateway"));
    }

    #[test]
    fn two_ready_gateways_have_no_findings_and_only_own_pods_count() {
        let pods = [
            pod("rook-ceph-nvmeof-nvmeof-a-1", true),
            pod("rook-ceph-nvmeof-nvmeof-b-2", true),
            pod("rook-ceph-nvmeof-other-a-3", true),
        ];
        let g = summarize_gateway(&cr("nvmeof", 2), &pods);
        assert_eq!(g["pods_ready"], 2);
        assert_eq!(g["pods"].as_array().unwrap().len(), 2);
        assert!(g["findings"].as_array().unwrap().is_empty());
    }

    #[test]
    fn missing_or_unready_pods_are_reported() {
        let g = summarize_gateway(&cr("nvmeof", 2), &[pod("rook-ceph-nvmeof-nvmeof-a-1", false)]);
        assert_eq!(g["pods_ready"], 0);
        assert!(g["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f.as_str().unwrap().contains("fewer gateway pods")));
    }

    fn ns_body(size: i64, confirm: bool) -> CreateNamespaceBody {
        CreateNamespaceBody {
            tenant_id: "t1".into(),
            name: "ns1".into(),
            namespace: None,
            storage_class: "nvmeof".into(),
            size_bytes: size,
            confirm,
        }
    }

    #[test]
    fn a_namespace_request_needs_confirm_and_a_sane_size() {
        assert!(validate_namespace_request(&ns_body(1 << 30, true)).is_ok());
        assert!(validate_namespace_request(&ns_body(1 << 30, false)).unwrap_err().contains("confirm"));
        assert!(validate_namespace_request(&ns_body(0, true)).is_err());
        assert!(validate_namespace_request(&ns_body(MAX_NAMESPACE_BYTES + 1, true)).is_err());
        let mut b = ns_body(1 << 30, true);
        b.storage_class = " ".into();
        assert!(validate_namespace_request(&b).is_err());
    }

    #[test]
    fn a_cr_without_instances_is_treated_as_one() {
        let bare = json!({ "metadata": { "name": "x" }, "spec": {} });
        assert_eq!(summarize_gateway(&bare, &[])["instances"], 1);
    }
}
