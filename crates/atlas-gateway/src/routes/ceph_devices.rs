// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `GET /ceph/devices`: the drives Ceph tracks, with the failure predictions its device-health
//! module has made, and a short list of the ones expected to fail soon.

use axum::{extract::State, Json};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

use atlas_common::{AppError, AppResult};

use super::util::CEPH_BACKEND_ID;
use crate::state::AppState;

/// A drive predicted to fail within this many days is listed as at risk.
const AT_RISK_DAYS: i64 = 90;

/// Ceph writes `2026-10-01T00:00:00.000000+0000`; an empty string means "no prediction".
fn parse_stamp(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    DateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f%z")
        .or_else(|_| DateTime::parse_from_rfc3339(raw))
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Summarize `ceph device ls` output as of `now`. A drive is at risk when Ceph predicts it fails
/// within [`AT_RISK_DAYS`], including one whose predicted window has already passed.
pub(crate) fn summarize(devices: &Value, now: DateTime<Utc>) -> Value {
    let list = devices.as_array().cloned().unwrap_or_default();
    let horizon = now + Duration::days(AT_RISK_DAYS);
    let mut with_prediction = 0usize;
    let mut at_risk = Vec::new();
    for d in &list {
        let min = d["life_expectancy_min"].as_str().and_then(parse_stamp);
        let max = d["life_expectancy_max"].as_str().and_then(parse_stamp);
        if min.is_none() && max.is_none() {
            continue;
        }
        with_prediction += 1;
        // The earliest date Ceph gives is the cautious one.
        let earliest = min.or(max).expect("one of them is set");
        if earliest <= horizon {
            at_risk.push(json!({
                "devid": d["devid"],
                "daemons": d["daemons"],
                "location": d["location"],
                "life_expectancy_min": d["life_expectancy_min"],
                "life_expectancy_max": d["life_expectancy_max"],
                "already_due": earliest <= now,
            }));
        }
    }
    json!({
        "total": list.len(),
        "with_prediction": with_prediction,
        "without_prediction": list.len() - with_prediction,
        "at_risk_within_days": AT_RISK_DAYS,
        "at_risk": at_risk,
    })
}

/// `GET /ceph/devices` — `ceph device ls` plus a summary of the drives predicted to fail soon.
/// Predictions exist only when Ceph's device-health module is on and has scraped the drive; a drive
/// without one is not known to be healthy, only not known to be failing.
pub(crate) async fn get_ceph_devices(State(s): State<AppState>) -> AppResult<Json<Value>> {
    let d = s
        .driver_for(CEPH_BACKEND_ID)
        .ok_or_else(|| AppError::Driver("no ceph driver".into()))?;
    let devices = d
        .ceph_device_ls()
        .await
        .map_err(|e| AppError::Driver(e.to_string()))?;
    Ok(Json(json!({
        "summary": summarize(&devices, Utc::now()),
        "devices": devices,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        parse_stamp(s).expect("valid stamp")
    }

    fn dev(id: &str, min: &str, max: &str) -> Value {
        json!({ "devid": id, "daemons": ["osd.1"], "location": [],
                "life_expectancy_min": min, "life_expectancy_max": max })
    }

    #[test]
    fn parses_cephs_timestamp_format_and_rfc3339() {
        assert!(parse_stamp("2026-10-01T00:00:00.000000+0000").is_some());
        assert!(parse_stamp("2026-10-01T00:00:00Z").is_some());
        assert_eq!(parse_stamp(""), None);
        assert_eq!(parse_stamp("soon"), None);
    }

    #[test]
    fn drives_without_a_prediction_are_counted_but_never_at_risk() {
        let now = at("2026-10-26T00:00:00Z");
        let s = summarize(&json!([dev("a", "", ""), dev("b", "", "")]), now);
        assert_eq!(s["total"], 2);
        assert_eq!(s["with_prediction"], 0);
        assert_eq!(s["without_prediction"], 2);
        assert!(s["at_risk"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_drive_expected_to_fail_inside_the_horizon_is_at_risk() {
        let now = at("2026-10-26T00:00:00Z");
        let soon = summarize(
            &json!([dev("soon", "2026-12-01T00:00:00Z", "2027-03-01T00:00:00Z")]),
            now,
        );
        assert_eq!(soon["at_risk"][0]["devid"], "soon");
        assert_eq!(soon["at_risk"][0]["already_due"], false);
        let late = summarize(
            &json!([dev("late", "2027-06-01T00:00:00Z", "2027-09-01T00:00:00Z")]),
            now,
        );
        assert!(late["at_risk"].as_array().unwrap().is_empty());
        assert_eq!(late["with_prediction"], 1);
    }

    #[test]
    fn the_earliest_date_decides_and_a_passed_window_is_already_due() {
        let now = at("2026-10-26T00:00:00Z");
        // Only the max is given.
        let only_max = summarize(&json!([dev("m", "", "2026-11-15T00:00:00Z")]), now);
        assert_eq!(only_max["at_risk"][0]["devid"], "m");
        let past = summarize(
            &json!([dev("p", "2020-01-01T00:00:00Z", "2020-06-01T00:00:00Z")]),
            now,
        );
        assert_eq!(past["at_risk"][0]["already_due"], true);
    }

    #[test]
    fn anything_that_is_not_a_list_summarizes_as_empty() {
        let s = summarize(&json!({"unexpected": true}), at("2026-10-26T00:00:00Z"));
        assert_eq!(s["total"], 0);
    }
}
