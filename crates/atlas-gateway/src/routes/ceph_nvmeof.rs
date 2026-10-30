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
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use atlas_common::{AppError, AppResult};
use atlas_driver_k8s::rook::PodBrief;

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

    #[test]
    fn a_cr_without_instances_is_treated_as_one() {
        let bare = json!({ "metadata": { "name": "x" }, "spec": {} });
        assert_eq!(summarize_gateway(&bare, &[])["instances"], 1);
    }
}
