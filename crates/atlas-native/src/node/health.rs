// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! What has failed for good on this node, and whether that calls for a restart
//! ([`crate::restart`]). `/healthz`, `/readyz`, `/metrics`, `/v1/status` and the binary's main
//! loop all ask here.

use serde::Serialize;

use super::NodeShared;
use crate::{data_node::DataNodeServer, http::Response, metrics::PromText, restart::DeviceRestart};

/// A component that stays failed until the process is restarted.
#[derive(Debug, Clone, Serialize)]
pub(super) struct Failure {
    /// `device<index>` or `metadata`.
    pub component: String,
    pub error: String,
    pub restart_wanted: bool,
}

#[derive(Debug, Default)]
pub(super) struct Health {
    pub failures: Vec<Failure>,
    /// The data node serves devices and none of them accepts writes.
    pub no_writable_device: bool,
}

impl Health {
    /// Why the process should be restarted, if it should.
    pub fn restart_reason(&self) -> Option<String> {
        let wanted: Vec<String> = self
            .failures
            .iter()
            .filter(|f| f.restart_wanted)
            .map(|f| format!("{}: {}", f.component, f.error))
            .collect();
        (!wanted.is_empty()).then(|| wanted.join("; "))
    }
}

pub(super) fn assess(sh: &NodeShared) -> Health {
    let (devices, count) = sh
        .data
        .lock()
        .ok()
        .and_then(|d| {
            d.as_ref()
                .map(|d: &DataNodeServer| (d.failed_devices(), d.device_count()))
        })
        .unwrap_or_default();
    let no_writable_device = count > 0 && devices.len() == count;
    let mut failures = Vec::new();
    for (index, error) in devices {
        let component = format!("device{index}");
        let restart_wanted = match sh.restarts.policy().devices {
            DeviceRestart::Any => sh.restarts.wanted(&component),
            DeviceRestart::Last => no_writable_device && sh.restarts.wanted(&component),
            DeviceRestart::Never => false,
        };
        failures.push(Failure {
            component,
            error,
            restart_wanted,
        });
    }
    if let Some(error) = sh.groups.iter().find_map(|g| {
        let e = g.raft.fatal_error()?;
        Some(format!("replica of group {} stopped: {e}", g.raft.group()))
    }) {
        failures.push(Failure {
            component: "metadata".into(),
            error,
            restart_wanted: sh.restarts.wanted("metadata"),
        });
    }
    Health {
        failures,
        no_writable_device,
    }
}

/// `/healthz`: 503 while a restart is wanted, so that a liveness probe performs it.
pub(super) fn liveness(sh: &NodeShared) -> Response {
    match assess(sh).restart_reason() {
        Some(reason) => Response::text(503, format!("restart required: {reason}")),
        None => Response::text(200, "ok"),
    }
}

pub(super) fn metrics(sh: &NodeShared) -> String {
    let health = assess(sh);
    let node = [("node", sh.id.as_str())];
    let mut p = PromText::new();
    let name = "atlas_native_restart_required";
    p.family(
        name,
        "gauge",
        "1 while /healthz fails because a restart is wanted after a storage failure.",
    )
    .sample(name, &node, u8::from(health.restart_reason().is_some()));
    if !sh.groups.is_empty() {
        let name = "atlas_native_metadata_failed";
        let failed = health.failures.iter().any(|f| f.component == "metadata");
        p.family(
            name,
            "gauge",
            "1 if a metadata replica stopped on a storage error.",
        )
        .sample(name, &node, u8::from(failed));
    }
    let name = "atlas_native_failed_without_restart";
    let family = p.family(
        name,
        "gauge",
        "Components that failed and for which no restart is wanted: someone has to act.",
    );
    for f in health.failures.iter().filter(|f| !f.restart_wanted) {
        family.sample(
            name,
            &[
                ("node", sh.id.as_str()),
                ("component", f.component.as_str()),
            ],
            1,
        );
    }
    let name = "atlas_native_failure_restarts";
    let family = p.family(
        name,
        "gauge",
        "Restarts asked for after storage failures within the window, kept across restarts.",
    );
    for (component, n) in sh.restarts.counted() {
        family.sample(
            name,
            &[("node", sh.id.as_str()), ("component", component.as_str())],
            n as u64,
        );
    }
    p.finish()
}
