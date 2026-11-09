// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! A real-mode provision whose operator CR apply fails (API server or admission webhook down)
//! must hand the plan back to `assessed`, so the next provision can run.

mod common;

const KUBECONFIG: &str = r#"apiVersion: v1
kind: Config
clusters:
- name: down
  cluster:
    server: https://127.0.0.1:1
    insecure-skip-tls-verify: true
contexts:
- name: down
  context: { cluster: down, user: down }
current-context: down
users:
- name: down
  user: { token: unused }
"#;

#[tokio::test]
async fn failed_cr_apply_leaves_the_plan_retryable() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let path = std::env::temp_dir().join(format!("atlas-kubeconfig-down-{}", std::process::id()));
    std::fs::write(&path, KUBECONFIG).unwrap();
    // This test binary holds only this test, so setting KUBECONFIG can't race another test.
    unsafe { std::env::set_var("KUBECONFIG", &path) };
    let k8s = atlas_driver_k8s::K8sDriver::try_default()
        .await
        .expect("client from kubeconfig");

    let url = common::fresh_database_url("provision-retry").await;
    let pool = atlas_inventory::connect(&url).await.unwrap();
    atlas_inventory::migrate(&pool, &url).await.unwrap();
    use atlas_inventory::databridge::{edge_clusters, plans, sources};
    sources::insert_source(
        &pool,
        "src_t",
        "global",
        "orders",
        "postgres",
        "generic",
        Some("db.example"),
        Some(5432),
        Some("orders"),
        Some("creds"),
        Some("zyvor-databridge"),
        "require",
        "real",
    )
    .await
    .unwrap();
    plans::insert_plan(&pool, "mplan_t", "global", "orders", "src_t", 3600)
        .await
        .unwrap();
    plans::set_state(&pool, "mplan_t", "assessed")
        .await
        .unwrap();

    for _ in 0..2 {
        let err = atlas_databridge::pipeline::provision_edge(&pool, Some(&k8s), "mplan_t")
            .await
            .expect_err("the API server is unreachable");
        assert!(err.to_string().contains("apply Cluster CR"), "{err}");
        let plan = plans::get_plan(&pool, "mplan_t").await.unwrap().unwrap();
        assert_eq!(plan.state, "assessed");
        assert!(edge_clusters::list_edge_clusters(&pool)
            .await
            .unwrap()
            .is_empty());
    }
    let _ = std::fs::remove_file(&path);
}
