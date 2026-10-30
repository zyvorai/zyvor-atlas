// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `GET /ceph/sizing`: do the hosts with NVMe drives have the CPU and RAM that Red Hat's sizing
//! rule of thumb for IOPS-optimized NVMe clusters asks for? Read-only.
//!
//! The thresholds are heuristics, not requirements: by default 10 cores per NVMe device, and
//! 16 GiB of RAM plus 5 GiB per OSD. Override them with `ATLAS_SIZING_CORES_PER_NVME`,
//! `ATLAS_SIZING_RAM_BASE_GIB` and `ATLAS_SIZING_RAM_PER_OSD_GIB`. CPU and RAM come from the
//! Kubernetes node's capacity, so without a cluster attached every host is `unknown`; a host whose
//! node cannot be matched is `unknown` too, never "undersized".

use std::collections::{BTreeMap, BTreeSet};

use axum::{extract::State, Json};
use serde_json::{json, Value};

use atlas_common::{AppError, AppResult};
use atlas_driver_k8s::rook::NodeCapacity;

use super::util::CEPH_BACKEND_ID;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Thresholds {
    pub cores_per_nvme: f64,
    pub ram_base_gib: f64,
    pub ram_per_osd_gib: f64,
}

impl Thresholds {
    pub(crate) fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let num = |key: &str, default: f64| {
            get(key)
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0)
                .unwrap_or(default)
        };
        Self {
            cores_per_nvme: num("ATLAS_SIZING_CORES_PER_NVME", 10.0),
            ram_base_gib: num("ATLAS_SIZING_RAM_BASE_GIB", 16.0),
            ram_per_osd_gib: num("ATLAS_SIZING_RAM_PER_OSD_GIB", 5.0),
        }
    }

    fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }
}

/// What a host carries: its distinct NVMe devices and its OSDs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HostLoad {
    pub host: String,
    pub nvme_devices: usize,
    pub osds: usize,
}

/// Group `ceph device ls` and `ceph osd metadata` by host. Only hosts with at least one NVMe
/// device are returned, since the rule of thumb is about NVMe clusters.
pub(crate) fn host_loads(device_ls: &Value, osd_meta: &Value) -> Vec<HostLoad> {
    let mut nvme: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for d in device_ls.as_array().into_iter().flatten() {
        let Some(devid) = d["devid"].as_str() else { continue };
        for l in d["location"].as_array().into_iter().flatten() {
            let is_nvme = l["dev"]
                .as_str()
                .is_some_and(|s| s.trim_start_matches("/dev/").starts_with("nvme"));
            if let (true, Some(host)) = (is_nvme, l["host"].as_str()) {
                nvme.entry(host.to_string()).or_default().insert(devid.to_string());
            }
        }
    }
    let mut osds: BTreeMap<String, usize> = BTreeMap::new();
    for o in osd_meta.as_array().into_iter().flatten() {
        if let Some(h) = o["hostname"].as_str() {
            *osds.entry(h.to_string()).or_insert(0) += 1;
        }
    }
    nvme.into_iter()
        .map(|(host, devs)| HostLoad {
            osds: osds.get(&host).copied().unwrap_or(0),
            nvme_devices: devs.len(),
            host,
        })
        .collect()
}

/// Judge one host. `status` is `unknown` without capacity, otherwise `ok` or `undersized` with the
/// shortfalls spelled out.
pub(crate) fn evaluate(load: &HostLoad, cap: Option<&NodeCapacity>, th: &Thresholds) -> Value {
    let need_cores = load.nvme_devices as f64 * th.cores_per_nvme;
    let need_ram = th.ram_base_gib + th.ram_per_osd_gib * load.osds as f64;
    let mut v = json!({
        "host": load.host,
        "nvme_devices": load.nvme_devices,
        "osds": load.osds,
        "needs_cpu_cores": need_cores,
        "needs_memory_gib": need_ram,
    });
    let Some(cap) = cap else {
        v["status"] = json!("unknown");
        return v;
    };
    let mut short = Vec::new();
    if cap.cpu_cores < need_cores {
        short.push(format!("{:.1} CPU cores, rule of thumb asks for {need_cores:.1}", cap.cpu_cores));
    }
    if cap.memory_gib < need_ram {
        short.push(format!("{:.1} GiB RAM, rule of thumb asks for {need_ram:.1}", cap.memory_gib));
    }
    v["node"] = json!(cap.node);
    v["has_cpu_cores"] = json!(cap.cpu_cores);
    v["has_memory_gib"] = json!(cap.memory_gib);
    v["status"] = json!(if short.is_empty() { "ok" } else { "undersized" });
    v["shortfalls"] = json!(short);
    v
}

async fn collect(s: &AppState) -> Result<Value, AppError> {
    let d = s
        .driver_for(CEPH_BACKEND_ID)
        .ok_or_else(|| AppError::Driver("no ceph driver".into()))?;
    let drv_err = |e: atlas_driver_core::DriverError| AppError::Driver(e.to_string());
    let devices = d.ceph_device_ls().await.map_err(drv_err)?;
    let meta = d.ceph_osd_metadata().await.map_err(drv_err)?;
    let th = Thresholds::from_env();
    let mut hosts = Vec::new();
    for load in host_loads(&devices, &meta) {
        let cap = match &s.k8s {
            // A failing lookup leaves the host `unknown`; the report never fails on it.
            Some(k) => k.node_capacity(&load.host).await.ok().flatten(),
            None => None,
        };
        hosts.push(evaluate(&load, cap.as_ref(), &th));
    }
    let count = |st: &str| hosts.iter().filter(|h| h["status"] == st).count();
    Ok(json!({
        "thresholds": {
            "cores_per_nvme_device": th.cores_per_nvme,
            "ram_base_gib": th.ram_base_gib,
            "ram_per_osd_gib": th.ram_per_osd_gib,
            "note": "heuristics, not requirements; compared against Kubernetes node capacity",
        },
        "summary": {
            "hosts": hosts.len(),
            "undersized": count("undersized"),
            "unknown": count("unknown"),
        },
        "hosts": hosts,
    }))
}

/// One line per undersized host, for the Ops Advisor. Best effort: any failure yields no lines.
pub(crate) async fn attention(s: &AppState) -> Vec<String> {
    let Ok(report) = collect(s).await else {
        return Vec::new();
    };
    report["hosts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|h| h["status"] == "undersized")
        .map(|h| {
            // Host names and shortfalls reach the advisor's LLM prompt; keep the host plain.
            let host: String = h["host"]
                .as_str()
                .unwrap_or("?")
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                .take(64)
                .collect();
            let why: Vec<&str> = h["shortfalls"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            format!("{host}: {}", why.join("; "))
        })
        .collect()
}

/// `GET /ceph/sizing`.
pub(crate) async fn get_ceph_sizing(State(s): State<AppState>) -> AppResult<Json<Value>> {
    Ok(Json(collect(&s).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atlas_driver_ceph::FakeCephDriver;
    use atlas_driver_core::StorageDriver;

    fn cap(cpu: f64, mem: f64) -> NodeCapacity {
        NodeCapacity { node: "n".into(), cpu_cores: cpu, memory_gib: mem }
    }

    fn th() -> Thresholds {
        Thresholds::from_lookup(|_| None)
    }

    #[test]
    fn thresholds_default_and_ignore_bad_overrides() {
        assert_eq!(th(), Thresholds { cores_per_nvme: 10.0, ram_base_gib: 16.0, ram_per_osd_gib: 5.0 });
        let t = Thresholds::from_lookup(|k| match k {
            "ATLAS_SIZING_CORES_PER_NVME" => Some("4".into()),
            "ATLAS_SIZING_RAM_BASE_GIB" => Some("many".into()),
            "ATLAS_SIZING_RAM_PER_OSD_GIB" => Some("-1".into()),
            _ => None,
        });
        assert_eq!(t, Thresholds { cores_per_nvme: 4.0, ram_base_gib: 16.0, ram_per_osd_gib: 5.0 });
    }

    #[test]
    fn hosts_are_grouped_and_non_nvme_hosts_are_left_out() {
        let ls = json!([
            { "devid": "a", "location": [{ "host": "h1", "dev": "nvme0n1" }] },
            { "devid": "a", "location": [{ "host": "h1", "dev": "nvme0n1" }] },
            { "devid": "b", "location": [{ "host": "h1", "dev": "nvme1n1" }] },
            { "devid": "c", "location": [{ "host": "h2", "dev": "sdb" }] },
        ]);
        let meta = json!([{ "hostname": "h1" }, { "hostname": "h1" }, { "hostname": "h1" }, { "hostname": "h2" }]);
        let l = host_loads(&ls, &meta);
        assert_eq!(l, vec![HostLoad { host: "h1".into(), nvme_devices: 2, osds: 3 }]);
    }

    #[test]
    fn a_host_is_judged_on_cpu_and_ram_and_unknown_without_capacity() {
        let load = HostLoad { host: "h1".into(), nvme_devices: 2, osds: 4 };
        // needs 20 cores and 16 + 5*4 = 36 GiB
        assert_eq!(evaluate(&load, Some(&cap(32.0, 128.0)), &th())["status"], "ok");
        let low_cpu = evaluate(&load, Some(&cap(8.0, 128.0)), &th());
        assert_eq!(low_cpu["status"], "undersized");
        assert_eq!(low_cpu["shortfalls"].as_array().unwrap().len(), 1);
        let both = evaluate(&load, Some(&cap(8.0, 16.0)), &th());
        assert_eq!(both["shortfalls"].as_array().unwrap().len(), 2);
        assert_eq!(evaluate(&load, None, &th())["status"], "unknown");
        // exactly meeting the rule is fine
        assert_eq!(evaluate(&load, Some(&cap(20.0, 36.0)), &th())["status"], "ok");
    }

    #[tokio::test]
    async fn the_fake_cluster_has_three_nvme_hosts() {
        let d = FakeCephDriver::new("bkd_ceph_lab");
        let l = host_loads(&d.ceph_device_ls().await.unwrap(), &d.ceph_osd_metadata().await.unwrap());
        assert_eq!(l.len(), 3);
        assert!(l.iter().all(|h| h.nvme_devices == 2 && h.osds == 2));
    }
}
