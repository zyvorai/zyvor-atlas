// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Operator custom-resource builders for the edge database targets. Each module produces a `spec`
//! JSON (applied via `atlas_driver_k8s::apply_cr`) and a readiness predicate over the CR `status`.

pub mod cnpg;
pub mod mysql_operator;
pub mod psmdb;
pub mod streaming;

#[cfg(test)]
mod tests {
    #[test]
    fn cnpg_spec_and_readiness() {
        let spec = super::cnpg::cluster_spec(1, "zyvor-rbd-prod", "zyvor-rbd-fast", 40, "appdb");
        assert_eq!(spec["storage"]["storageClass"], "zyvor-rbd-prod");
        assert_eq!(spec["storage"]["size"], "40Gi");
        assert_eq!(spec["postgresql"]["parameters"]["wal_level"], "logical");
        assert!(super::cnpg::is_ready(
            &serde_json::json!({ "readyInstances": 1, "phase": "Cluster in healthy state" })
        ));
        assert!(!super::cnpg::is_ready(
            &serde_json::json!({ "readyInstances": 0 })
        ));
    }

    #[test]
    fn mysql_spec_and_readiness() {
        let spec = super::mysql_operator::cluster_spec(1, "zyvor-rbd-prod", 40);
        assert_eq!(
            spec["pxc"]["volumeSpec"]["persistentVolumeClaim"]["storageClassName"],
            "zyvor-rbd-prod"
        );
        assert!(super::mysql_operator::is_ready(
            &serde_json::json!({ "state": "ready" })
        ));
        assert!(!super::mysql_operator::is_ready(
            &serde_json::json!({ "state": "initializing" })
        ));
    }
}
