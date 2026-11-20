// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! CloudNativePG `Cluster` CR builder for the Postgres edge target. Data + WAL land on Ceph RBD
//! StorageClasses; `wal_level=logical` so Debezium can read the WAL for CDC.

pub const GROUP: &str = "postgresql.cnpg.io";
pub const VERSION: &str = "v1";
pub const KIND: &str = "Cluster";

/// Build the `spec` for a CloudNativePG Cluster. `size_gib` sizes the data volume; WAL gets ~10%.
pub fn cluster_spec(
    instances: i64,
    storage_class: &str,
    wal_storage_class: &str,
    size_gib: i64,
    database: &str,
) -> serde_json::Value {
    let wal_gib = (size_gib / 10).max(2);
    serde_json::json!({
        "instances": instances.max(1),
        "imageName": "ghcr.io/cloudnative-pg/postgresql:16",
        "storage": { "size": format!("{size_gib}Gi"), "storageClass": storage_class },
        "walStorage": { "size": format!("{wal_gib}Gi"), "storageClass": wal_storage_class },
        "postgresql": { "parameters": { "wal_level": "logical", "max_replication_slots": "10" } },
        "bootstrap": { "initdb": { "database": database, "owner": "app" } }
    })
}

/// CNPG reports readiness via `status.readyInstances` and a healthy `status.phase`.
pub fn is_ready(status: &serde_json::Value) -> bool {
    let ready = status
        .get("readyInstances")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let phase = status.get("phase").and_then(|p| p.as_str()).unwrap_or("");
    ready >= 1 && phase.to_lowercase().contains("healthy")
}

/// Read-write service + generated app Secret follow CNPG's `<cluster>-rw` / `<cluster>-app` naming.
pub fn endpoint(cr_name: &str, namespace: &str) -> String {
    format!("{cr_name}-rw.{namespace}.svc:5432")
}
pub fn secret_ref(cr_name: &str) -> String {
    format!("{cr_name}-app")
}
