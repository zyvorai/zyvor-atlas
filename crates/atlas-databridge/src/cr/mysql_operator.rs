// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Percona XtraDB Cluster CR builder for the MySQL edge target. Data lands on a Ceph RBD
//! StorageClass; binlog is enabled by the operator image so Debezium can read it for CDC.

pub const GROUP: &str = "pxc.percona.com";
pub const VERSION: &str = "v1";
pub const KIND: &str = "PerconaXtraDBCluster";

/// Build the `spec` for a PerconaXtraDBCluster with the PXC data volume on `storage_class`.
pub fn cluster_spec(instances: i64, storage_class: &str, size_gib: i64) -> serde_json::Value {
    let size = instances.max(1);
    serde_json::json!({
        "crVersion": "1.14.0",
        // Galera quorum normally wants >=3 members; the operator refuses to ever report
        // status.state == "ready" for a smaller cluster unless this is set. Atlas provisions
        // single-node (size=1) edge targets today, so without this every real-mode MySQL edge
        // cluster would sit at "initializing" forever and full-load/CDC could never start.
        "allowUnsafeConfigurations": size < 3,
        "pxc": {
            "size": size,
            "image": "percona/percona-xtradb-cluster:8.0",
            "volumeSpec": {
                "persistentVolumeClaim": {
                    "storageClassName": storage_class,
                    "resources": { "requests": { "storage": format!("{size_gib}Gi") } }
                }
            }
        },
        "haproxy": {
            "enabled": true,
            "size": 1,
            "image": "percona/percona-xtradb-cluster-operator:1.14.0-haproxy"
        }
    })
}

/// Percona reports readiness via `status.state == "ready"`.
pub fn is_ready(status: &serde_json::Value) -> bool {
    status
        .get("state")
        .and_then(|s| s.as_str())
        .map(|s| s.eq_ignore_ascii_case("ready"))
        .unwrap_or(false)
}

pub fn endpoint(cr_name: &str, namespace: &str) -> String {
    format!("{cr_name}-haproxy.{namespace}.svc:3306")
}
pub fn secret_ref(cr_name: &str) -> String {
    format!("{cr_name}-secrets")
}
