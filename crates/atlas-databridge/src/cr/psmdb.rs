// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Percona Server for MongoDB (`PerconaServerMongoDB`) CR builder for the MongoDB edge target. Data
//! lands on a Ceph RBD StorageClass. The edge is a **replica set** (`rs0`) so Debezium change streams
//! / the oplog are available for CDC — a standalone mongod can't serve change streams.

pub const GROUP: &str = "psmdb.percona.com";
pub const VERSION: &str = "v1";
pub const KIND: &str = "PerconaServerMongoDB";

/// The replica-set name PSMDB uses by default; also the `?replicaSet=` for connection strings.
pub const REPLICA_SET: &str = "rs0";

/// Build the `spec` for a PerconaServerMongoDB replica set with its data volume on `storage_class`.
pub fn cluster_spec(instances: i64, storage_class: &str, size_gib: i64) -> serde_json::Value {
    serde_json::json!({
        "crVersion": "1.16.0",
        "image": "percona/percona-server-mongodb:7.0",
        "unsafeFlags": { "replsetSize": true },
        "replsets": [{
            "name": REPLICA_SET,
            "size": instances.max(1),
            "volumeSpec": {
                "persistentVolumeClaim": {
                    "storageClassName": storage_class,
                    "resources": { "requests": { "storage": format!("{size_gib}Gi") } }
                }
            }
        }],
        // Single-node lab: no sharding.
        "sharding": { "enabled": false }
    })
}

/// PSMDB reports readiness via `status.state == "ready"`.
pub fn is_ready(status: &serde_json::Value) -> bool {
    status
        .get("state")
        .and_then(|s| s.as_str())
        .map(|s| s.eq_ignore_ascii_case("ready"))
        .unwrap_or(false)
}

/// The replica-set headless service PSMDB exposes: `<cluster>-rs0.<namespace>.svc:27017`.
pub fn endpoint(cr_name: &str, namespace: &str) -> String {
    format!("{cr_name}-{REPLICA_SET}.{namespace}.svc:27017")
}

/// PSMDB generates a users Secret named `internal-<cluster>-users` (keys
/// `MONGODB_DATABASE_ADMIN_USER` / `MONGODB_DATABASE_ADMIN_PASSWORD`). Older docs called this
/// `<cluster>-secrets`; current operators (1.16+) use the `internal-…-users` name.
pub fn secret_ref(cr_name: &str) -> String {
    format!("internal-{cr_name}-users")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_puts_data_on_storage_class_and_is_a_replica_set() {
        let spec = cluster_spec(1, "zyvor-rbd-prod", 40);
        assert_eq!(spec["replsets"][0]["name"], "rs0");
        assert_eq!(
            spec["replsets"][0]["volumeSpec"]["persistentVolumeClaim"]["storageClassName"],
            "zyvor-rbd-prod"
        );
        assert_eq!(
            spec["replsets"][0]["volumeSpec"]["persistentVolumeClaim"]["resources"]["requests"]
                ["storage"],
            "40Gi"
        );
    }

    #[test]
    fn readiness_and_naming() {
        assert!(is_ready(&serde_json::json!({ "state": "ready" })));
        assert!(!is_ready(&serde_json::json!({ "state": "initializing" })));
        assert_eq!(
            endpoint("edge-abc", "zyvor-databridge"),
            "edge-abc-rs0.zyvor-databridge.svc:27017"
        );
        assert_eq!(secret_ref("edge-abc"), "internal-edge-abc-users");
    }
}
