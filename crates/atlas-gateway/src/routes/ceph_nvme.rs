// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `GET /ceph/nvme`: wear and health of the NVMe drives Ceph tracks, read from the SMART scrapes
//! its device-health module holds (`ceph device get-health-metrics`). Read-only. A drive Ceph has
//! not scraped is reported without metrics, never as healthy.

use axum::{extract::State, Json};
use serde_json::{json, Value};

use atlas_common::{AppError, AppResult};
use atlas_driver_core::StorageDriver;

use super::util::CEPH_BACKEND_ID;
use crate::state::AppState;

/// At most this many drives are queried per request (one `ceph` call each).
const MAX_DEVICES: usize = 64;
/// `percentage_used` at or above this is reported as worn.
const WORN_PERCENT: f64 = 80.0;

/// The NVMe drives in `ceph device ls` output as `(devid, host, dev)`; a drive counts as NVMe when
/// any of its locations is an `nvme*` device.
fn nvme_drives(devices: &Value) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for d in devices.as_array().into_iter().flatten() {
        let Some(devid) = d["devid"].as_str() else {
            continue;
        };
        let nvme = d["location"].as_array().into_iter().flatten().find(|l| {
            l["dev"]
                .as_str()
                .is_some_and(|s| s.trim_start_matches("/dev/").starts_with("nvme"))
        });
        if let Some(l) = nvme {
            out.push((
                devid.to_string(),
                l["host"].as_str().unwrap_or_default().to_string(),
                l["dev"].as_str().unwrap_or_default().to_string(),
            ));
        }
    }
    out
}

/// The newest scrape's `nvme_smart_health_information_log`. Scrapes are keyed by timestamp
/// (`YYYYMMDD-HHMMSS`), which sorts lexically.
fn latest_log(metrics: &Value) -> Option<&Value> {
    metrics
        .as_object()?
        .iter()
        .filter_map(|(k, v)| v.get("nvme_smart_health_information_log").map(|l| (k, l)))
        .max_by(|a, b| a.0.cmp(b.0))
        .map(|(_, l)| l)
}

/// What is wrong with a drive, from its SMART log. Empty means nothing found, not "certified".
fn concerns(log: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let num = |k: &str| log[k].as_f64();
    if let Some(w) = num("critical_warning").filter(|w| *w != 0.0) {
        out.push(format!("critical warning flags set ({w})"));
    }
    if let Some(u) = num("percentage_used").filter(|u| *u >= WORN_PERCENT) {
        out.push(format!("{u}% of rated endurance used"));
    }
    if let Some(e) = num("media_errors").filter(|e| *e > 0.0) {
        out.push(format!("{e} media error(s)"));
    }
    if let (Some(s), Some(t)) = (num("available_spare"), num("available_spare_threshold")) {
        if s <= t {
            out.push(format!("available spare {s}% at or below threshold {t}%"));
        }
    }
    out
}

fn report(devid: &str, host: &str, dev: &str, daemons: &Value, metrics: &Value) -> Value {
    match latest_log(metrics) {
        Some(log) => json!({
            "devid": devid, "host": host, "dev": dev, "daemons": daemons,
            "scraped": true,
            "wear_percent": log["percentage_used"],
            "available_spare_percent": log["available_spare"],
            "media_errors": log["media_errors"],
            "temperature_c": log["temperature"],
            "critical_warning": log["critical_warning"],
            "concerns": concerns(log),
        }),
        None => json!({
            "devid": devid, "host": host, "dev": dev, "daemons": daemons,
            "scraped": false, "concerns": [],
        }),
    }
}

/// Collect the NVMe report from a Ceph driver. A drive whose metrics cannot be read is listed as
/// not scraped (with the reason) rather than failing the whole report.
pub(crate) async fn collect(d: &dyn StorageDriver) -> Result<Value, atlas_driver_core::DriverError> {
    let listing = d.ceph_device_ls().await?;
    let drives = nvme_drives(&listing);
    let truncated = drives.len() > MAX_DEVICES;
    let mut devices = Vec::new();
    for (devid, host, dev) in drives.into_iter().take(MAX_DEVICES) {
        let daemons = listing
            .as_array()
            .into_iter()
            .flatten()
            .find(|x| x["devid"] == devid.as_str())
            .map(|x| x["daemons"].clone())
            .unwrap_or(Value::Null);
        match d.ceph_device_health(&devid).await {
            Ok(m) => devices.push(report(&devid, &host, &dev, &daemons, &m)),
            Err(e) => {
                let mut r = report(&devid, &host, &dev, &daemons, &Value::Null);
                r["error"] = json!(e.to_string());
                devices.push(r);
            }
        }
    }
    let count = |f: &dyn Fn(&Value) -> bool| devices.iter().filter(|v| f(v)).count();
    Ok(json!({
        "summary": {
            "total": devices.len(),
            "scraped": count(&|v| v["scraped"] == true),
            "with_concerns": count(&|v| !v["concerns"].as_array().is_none_or(|c| c.is_empty())),
            "worn_at_percent": WORN_PERCENT,
            "truncated": truncated,
        },
        "devices": devices,
    }))
}

/// One line per drive with a concern, for the Ops Advisor. Best effort: any failure (backend with
/// no Ceph, `ceph` unreachable) yields an empty list, because the advisor must not fail on it.
pub(crate) async fn attention(s: &AppState) -> Vec<String> {
    let Some(d) = s.driver_for(CEPH_BACKEND_ID) else {
        return Vec::new();
    };
    let Ok(report) = collect(d.as_ref()).await else {
        return Vec::new();
    };
    report["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let c: Vec<&str> = v["concerns"].as_array()?.iter().filter_map(Value::as_str).collect();
            // The devid comes from drive firmware and the line reaches the advisor's LLM prompt,
            // so keep it to a bounded, plain identifier.
            let id: String = v["devid"]
                .as_str()
                .unwrap_or("?")
                .chars()
                .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
                .take(64)
                .collect();
            (!c.is_empty()).then(|| format!("{id}: {}", c.join("; ")))
        })
        .collect()
}

/// `GET /ceph/nvme` — NVMe wear and health per drive, from Ceph's device-health scrapes.
pub(crate) async fn get_ceph_nvme(State(s): State<AppState>) -> AppResult<Json<Value>> {
    let d = s
        .driver_for(CEPH_BACKEND_ID)
        .ok_or_else(|| AppError::Driver("no ceph driver".into()))?;
    let report = collect(d.as_ref())
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    Ok(Json(report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atlas_driver_ceph::FakeCephDriver;

    #[test]
    fn only_nvme_locations_are_listed() {
        let ls = json!([
            { "devid": "a", "location": [{ "host": "h1", "dev": "nvme0n1" }], "daemons": ["osd.0"] },
            { "devid": "b", "location": [{ "host": "h1", "dev": "sdb" }], "daemons": ["osd.1"] },
            { "devid": "c", "location": [], "daemons": [] },
        ]);
        let d = nvme_drives(&ls);
        assert_eq!(d, vec![("a".into(), "h1".into(), "nvme0n1".into())]);
    }

    #[test]
    fn newest_scrape_wins() {
        let m = json!({
            "20260101-000000": { "nvme_smart_health_information_log": { "percentage_used": 1 } },
            "20260301-000000": { "nvme_smart_health_information_log": { "percentage_used": 9 } },
            "20260201-000000": { "other": {} },
        });
        assert_eq!(latest_log(&m).unwrap()["percentage_used"], 9);
        assert!(latest_log(&json!({})).is_none());
        assert!(latest_log(&Value::Null).is_none());
    }

    #[test]
    fn concerns_flag_wear_errors_spare_and_warnings() {
        assert!(concerns(&json!({ "percentage_used": 79, "available_spare": 100,
            "available_spare_threshold": 10, "media_errors": 0, "critical_warning": 0 }))
        .is_empty());
        assert_eq!(concerns(&json!({ "percentage_used": 80 })).len(), 1);
        assert_eq!(concerns(&json!({ "media_errors": 2 })).len(), 1);
        assert_eq!(concerns(&json!({ "available_spare": 10, "available_spare_threshold": 10 })).len(), 1);
        assert_eq!(concerns(&json!({ "critical_warning": 4 })).len(), 1);
    }

    #[tokio::test]
    async fn fake_cluster_reports_two_unhealthy_drives() {
        let d = FakeCephDriver::new("bkd_ceph_lab");
        let r = collect(&d).await.unwrap();
        assert_eq!(r["summary"]["total"], 6);
        assert_eq!(r["summary"]["scraped"], 6);
        assert_eq!(r["summary"]["with_concerns"], 2);
        let worn = r["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["devid"] == "SAMSUNG_MZQL23T8HCLS_S64E0002")
            .unwrap();
        assert_eq!(worn["wear_percent"], 92);
        assert_eq!(worn["concerns"].as_array().unwrap().len(), 2);
    }
}
