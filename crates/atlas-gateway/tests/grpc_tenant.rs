// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! gRPC edge authorization: a revoked token is refused, and a non-admin caller only sees (and
//! may only act on) its own tenant's volumes, snapshots and buckets. Mirrors the REST rules.

use atlas_api_types::{StorageVolume, VolumeKind};
use atlas_common::config::CephDriverMode;
use atlas_common::Config;
use atlas_gateway::proto::atlas_storage_client::AtlasStorageClient;
use atlas_gateway::proto::{
    CreateVolumeRequest, Empty, ExpandVolumeRequest, GetVolumeRequest, HealthRequest,
};
use atlas_gateway::startup::{build_state, BuildOptions};
use tonic::Request;

mod common;

const SECRET: &str = "grpc-tenant-secret";

fn vol(id: &str) -> StorageVolume {
    StorageVolume {
        id: id.into(),
        cluster_id: None,
        pool_id: None,
        name: id.into(),
        kind: VolumeKind::Block,
        backend_native_id: None,
        size_bytes: 1 << 30,
        used_bytes: None,
        state: "available".into(),
        health: Default::default(),
        kubernetes_namespace: Some("default".into()),
        pvc_name: Some(id.into()),
        storage_class_name: None,
    }
}

fn with_token<T>(msg: T, token: &str) -> Request<T> {
    let mut r = Request::new(msg);
    r.metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    r
}

#[tokio::test]
async fn grpc_revocation_and_tenant_isolation() {
    let database_url = common::fresh_database_url("grpc-tenant").await;
    let config = Config {
        bind_addr: "127.0.0.1:0".into(),
        grpc_addr: "127.0.0.1:0".into(),
        database_url,
        ceph_driver_mode: CephDriverMode::Fake,
        kubeconfig_path: None,
        jwt_secret: SECRET.into(),
        jwt_secret_previous: None,
        auth_required: true,
        bootstrap_admin_token: None,
        admin_username: "admin".into(),
        admin_password: "Admin@321".into(),
        monitor_interval_secs: 0,
        ceph_prometheus_url: None,
        alert_webhook_url: None,
        backup_keep: 0,
        backup_max_age_secs: 0,
        rgw_public_endpoint: None,
        snapshot_tick_secs: 0,
        databridge_reconcile_secs: 0,
        job_poll_secs: 0,
        job_stale_secs: 0,
        https_addr: None,
        tls_cert_path: None,
        tls_key_path: None,
        tls_self_signed: false,
        disable_http: false,
        nfs_enable: false,
        nfs_server: None,
        nfs_exports: Vec::new(),
        nfs_driver_mode: atlas_common::config::DriverMode::Fake,
        zfs_enable: false,
        zfs_host: None,
        zfs_pools: Vec::new(),
        zfs_driver_mode: atlas_common::config::DriverMode::Fake,
        oidc: None,
        rook_namespace: "rook-ceph".into(),
        rook_cluster_name: "rook-ceph".into(),
        dr_dataplane_verified: false,
    };
    let state = build_state(
        config,
        BuildOptions {
            enable_k8s: false,
            initial_discovery: false,
            enable_monitor: false,
        },
    )
    .await
    .unwrap();
    let pool = state.pool.clone();
    atlas_inventory::upsert_volume(&pool, "bkd_ceph_lab", "acme", &vol("vol_acme"), None)
        .await
        .unwrap();
    atlas_inventory::upsert_volume(&pool, "bkd_ceph_lab", "globex", &vol("vol_globex"), None)
        .await
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(atlas_gateway::grpc::service(state))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let mut client = AtlasStorageClient::connect(format!("http://{addr}"))
        .await
        .unwrap();

    let (acme_op, _, acme_jti) =
        atlas_gateway::auth::mint_token(SECRET, "acme-op", "operator", "acme", 3600).unwrap();
    let (admin, _, _) =
        atlas_gateway::auth::mint_token(SECRET, "root", "admin", "global", 3600).unwrap();

    // A tenant operator lists only its own tenant's volumes; an admin lists both.
    let mine = client
        .list_volumes(with_token(Empty {}, &acme_op))
        .await
        .unwrap()
        .into_inner();
    let ids: Vec<_> = mine.volumes.iter().map(|v| v.id.as_str()).collect();
    assert_eq!(ids, ["vol_acme"]);
    let all = client
        .list_volumes(with_token(Empty {}, &admin))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(all.volumes.len(), 2);

    // Another tenant's volume looks missing (not forbidden) for get and expand.
    let err = client
        .get_volume(with_token(
            GetVolumeRequest {
                id: "vol_globex".into(),
            },
            &acme_op,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
    client
        .get_volume(with_token(
            GetVolumeRequest {
                id: "vol_acme".into(),
            },
            &acme_op,
        ))
        .await
        .unwrap();
    let err = client
        .expand_volume(with_token(
            ExpandVolumeRequest {
                id: "vol_globex".into(),
                new_size_bytes: 2 << 30,
            },
            &acme_op,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);

    // Creating for another tenant is refused.
    let err = client
        .create_volume(with_token(
            CreateVolumeRequest {
                tenant_id: "globex".into(),
                name: "sneaky".into(),
                size_bytes: 1 << 30,
                policy: "database".into(),
                namespace: "default".into(),
                owner: None,
            },
            &acme_op,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    // A revoked token is refused on its next call.
    client
        .health(with_token(HealthRequest {}, &acme_op))
        .await
        .unwrap();
    atlas_inventory::tokens::revoke(&pool, &acme_jti, "test")
        .await
        .unwrap();
    let err = client
        .health(with_token(HealthRequest {}, &acme_op))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    assert!(err.message().contains("revoked"));
}
