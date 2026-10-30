// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `GET /ceph/osd-objectstore`: which object store and Ceph version each OSD reports, from
//! `ceph osd metadata`. Read-only and descriptive: it reports what the OSDs say and does not infer
//! anything about the OSD implementation (for example Crimson) from it.

use std::collections::BTreeMap;

use axum::{extract::State, Json};
use serde_json::{json, Value};

use atlas_common::{AppError, AppResult};

use super::util::CEPH_BACKEND_ID;
use crate::state::AppState;

fn count_by<'a>(values: impl Iterator<Item = &'a str>) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for v in values {
        *m.entry(v.to_string()).or_insert(0) += 1;
    }
    m
}

/// Per-OSD rows plus counts by object store and version. A key an OSD does not report is `null` in
/// its row and counted as `unknown`; `mixed_versions` considers only the versions that are known.
pub(crate) fn summarize(meta: &Value) -> Value {
    let list = meta.as_array().cloned().unwrap_or_default();
    let osds: Vec<Value> = list
        .iter()
        .map(|o| {
            json!({
                "id": o["id"],
                "hostname": o["hostname"],
                "osd_objectstore": o["osd_objectstore"],
                "ceph_version": o["ceph_version"],
            })
        })
        .collect();
    let stores = count_by(list.iter().map(|o| o["osd_objectstore"].as_str().unwrap_or("unknown")));
    let versions = count_by(list.iter().map(|o| o["ceph_version"].as_str().unwrap_or("unknown")));
    let known_versions = versions.keys().filter(|v| v.as_str() != "unknown").count();
    json!({
        "summary": {
            "total": list.len(),
            "by_objectstore": stores,
            "by_version": versions,
            "mixed_versions": known_versions > 1,
        },
        "osds": osds,
    })
}

/// `GET /ceph/osd-objectstore`.
pub(crate) async fn get_ceph_osd_objectstore(State(s): State<AppState>) -> AppResult<Json<Value>> {
    let d = s
        .driver_for(CEPH_BACKEND_ID)
        .ok_or_else(|| AppError::Driver("no ceph driver".into()))?;
    let meta = d
        .ceph_osd_metadata()
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    Ok(Json(summarize(&meta)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atlas_driver_ceph::FakeCephDriver;
    use atlas_driver_core::StorageDriver;

    #[test]
    fn counts_stores_and_versions_and_flags_mixed_versions() {
        let meta = json!([
            { "id": 0, "hostname": "a", "osd_objectstore": "bluestore", "ceph_version": "v1" },
            { "id": 1, "hostname": "a", "osd_objectstore": "bluestore", "ceph_version": "v2" },
            { "id": 2, "hostname": "b" },
        ]);
        let r = summarize(&meta);
        assert_eq!(r["summary"]["total"], 3);
        assert_eq!(r["summary"]["by_objectstore"]["bluestore"], 2);
        assert_eq!(r["summary"]["by_objectstore"]["unknown"], 1);
        assert_eq!(r["summary"]["mixed_versions"], true);
        assert!(r["osds"][2]["osd_objectstore"].is_null());
    }

    #[test]
    fn unknown_versions_alone_are_not_mixed() {
        let r = summarize(&json!([{ "id": 0 }, { "id": 1, "ceph_version": "v1" }]));
        assert_eq!(r["summary"]["mixed_versions"], false);
        assert_eq!(summarize(&json!({"not": "a list"}))["summary"]["total"], 0);
    }

    #[tokio::test]
    async fn the_fake_cluster_is_mid_upgrade() {
        let d = FakeCephDriver::new("bkd_ceph_lab");
        let r = summarize(&d.ceph_osd_metadata().await.unwrap());
        assert_eq!(r["summary"]["total"], 6);
        assert_eq!(r["summary"]["mixed_versions"], true);
    }
}
