// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Atlas gateway binary: REST + gRPC edges of the storage control plane.

use std::net::SocketAddr;

use atlas_common::Config;
use atlas_gateway::startup::{build_state, resolve_vault_secrets, BuildOptions};
use atlas_gateway::{grpc, routes};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Pin rustls's process-wide crypto provider to `ring` before anything builds a TLS client (the
    // Kubernetes client in build_state, outbound reqwest, the HTTPS/gRPC listeners below). Without
    // this rustls falls back to guessing from crate features, and it panics when more than one
    // provider is enabled — which is exactly what happened after the reqwest 0.13 upgrade: its
    // default rustls backend uses `aws-lc-rs` while kube/tonic here use `ring`, so both features
    // were on and the gateway crashed at startup building the k8s client. The install calls further
    // down (HTTPS/gRPC TLS branches) are now redundant but harmless (idempotent).
    let _ = rustls::crypto::ring::default_provider().install_default();

    atlas_common::init_tracing();

    let mut config = Config::from_env();
    // Before validate_for_start(): a Vault-sourced strong secret must not be checked against the
    // pre-resolution (possibly still dev-default) value.
    if let Err(e) = resolve_vault_secrets(&mut config).await {
        anyhow::bail!("failed to resolve secrets from Vault: {e:#}");
    }
    if let Err(msg) = config.validate_for_start() {
        anyhow::bail!("{msg}");
    }
    info!(?config, "starting atlas-gateway");
    if config.jwt_secret_is_weak() {
        tracing::warn!(
            "ATLAS_JWT_SECRET is the dev default or shorter than 32 bytes — set a strong secret in production"
        );
    }
    if config.auth_required && config.bootstrap_admin_token.is_some() {
        tracing::warn!(
            "ATLAS_BOOTSTRAP_ADMIN_TOKEN is set — use it once to mint service-account JWTs, then remove it from the Secret"
        );
    }

    let bind_addr = config.bind_addr.clone();
    let grpc_addr = config.grpc_addr.clone();
    let https_addr = config.https_addr.clone();
    let tls_cert = config.tls_cert_path.clone();
    let tls_key = config.tls_key_path.clone();
    // gRPC reuses the same cert/key as the REST HTTPS listener rather than introducing a
    // separate set of env vars — cloned here since the HTTPS block below consumes tls_cert/key.
    let grpc_tls = tls_cert.clone().zip(tls_key.clone());
    let disable_http = config.disable_http;
    let state = build_state(config, BuildOptions::default()).await?;
    atlas_gateway::routes::ransomware::spawn_autoprotect(state.clone());

    let app = routes::router(state.clone())
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive());

    // Plain HTTP REST listener. Skippable via ATLAS_DISABLE_HTTP once HTTPS is confirmed
    // working, so TLS can't be bypassed by hitting the HTTP port directly —
    // validate_for_start() already refused to allow this without a working HTTPS listener
    // configured alongside it.
    let rest_task: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = if disable_http {
        info!("plain HTTP REST listener disabled (ATLAS_DISABLE_HTTP=1) — HTTPS only");
        None
    } else {
        let addr: SocketAddr = bind_addr.parse()?;
        let listener = tokio::net::TcpListener::bind(addr).await?;
        info!("atlas-gateway REST listening on http://{addr}");
        let rest_app = app.clone();
        Some(tokio::spawn(async move {
            axum::serve(listener, rest_app)
                .with_graceful_shutdown(shutdown_signal())
                .await
                .map_err(anyhow::Error::from)
        }))
    };

    // Optional HTTPS listener (same app), enabled by ATLAS_HTTPS_ADDR + cert/key PEM paths.
    let https_task = if let (Some(haddr), Some(cert), Some(key)) = (https_addr, tls_cert, tls_key) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
            .await
            .map_err(|e| anyhow::anyhow!("load TLS cert/key: {e}"))?;
        let saddr: SocketAddr = haddr.parse()?;
        info!("atlas-gateway REST (TLS) listening on https://{saddr}");
        // Wire the same shutdown signal as the REST/gRPC listeners so in-flight HTTPS requests
        // drain instead of being killed when the process exits.
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(30)));
        });
        Some(tokio::spawn(async move {
            axum_server::bind_rustls(saddr, tls)
                .handle(handle)
                .serve(app.into_make_service())
                .await
                .map_err(anyhow::Error::from)
        }))
    } else {
        None
    };

    // gRPC edge (served concurrently). Empty ATLAS_GRPC_ADDR disables it. TLS (same cert/key as
    // the REST HTTPS listener) is used automatically when configured — previously this edge had
    // no TLS option at all, plaintext-only regardless of how REST was configured.
    let grpc_task: Option<tokio::task::JoinHandle<anyhow::Result<()>>> =
        if grpc_addr.trim().is_empty() {
            None
        } else {
            let gaddr: SocketAddr = grpc_addr.parse()?;
            let reflection = tonic_reflection::server::Builder::configure()
                .register_encoded_file_descriptor_set(atlas_gateway::proto::FILE_DESCRIPTOR_SET)
                .build_v1()?;
            let mut builder = tonic::transport::Server::builder();
            if let Some((cert, key)) = grpc_tls {
                // Idempotent: a no-op if the REST HTTPS listener above already installed it, but gRPC
                // TLS can in principle be configured without ATLAS_HTTPS_ADDR set, so don't rely on
                // that side effect.
                let _ = rustls::crypto::ring::default_provider().install_default();
                let cert_pem = tokio::fs::read(&cert)
                    .await
                    .map_err(|e| anyhow::anyhow!("read gRPC TLS cert {cert}: {e}"))?;
                let key_pem = tokio::fs::read(&key)
                    .await
                    .map_err(|e| anyhow::anyhow!("read gRPC TLS key {key}: {e}"))?;
                let identity = tonic::transport::Identity::from_pem(cert_pem, key_pem);
                builder = builder
                    .tls_config(tonic::transport::ServerTlsConfig::new().identity(identity))
                    .map_err(|e| anyhow::anyhow!("configure gRPC TLS: {e}"))?;
                info!("atlas-gateway gRPC (TLS) listening on {gaddr}");
            } else {
                info!("atlas-gateway gRPC listening on {gaddr}");
            }
            Some(tokio::spawn(async move {
                builder
                    .add_service(grpc::service(state))
                    .add_service(reflection)
                    .serve_with_shutdown(gaddr, shutdown_signal())
                    .await
                    .map_err(anyhow::Error::from)
            }))
        };

    for task in [rest_task, https_task, grpc_task].into_iter().flatten() {
        if let Err(e) = task.await? {
            tracing::error!("server task error: {e:#}");
        }
    }
    info!("atlas-gateway shut down cleanly");
    Ok(())
}

/// Resolves when the process receives SIGINT (Ctrl-C) or SIGTERM (k8s rollout/`docker stop`). Both
/// server tasks await their own copy so in-flight requests drain instead of being killed mid-flight.
/// An interrupted background job is recovered (failed-safe + re-enqueued) on the next start.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    info!("shutdown signal received — draining in-flight requests");
}
