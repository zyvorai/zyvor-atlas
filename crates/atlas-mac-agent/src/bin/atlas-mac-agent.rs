// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Per-Mac agent. Fake mode (recorded fixtures) is the default so nothing touches a real disk
//! unless `--mode real`. A non-loopback bind requires a token and TLS.

use std::net::SocketAddr;

use anyhow::{bail, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "atlas-mac-agent", about = "Serves this Mac's disk inventory to an Atlas gateway")]
struct Cli {
    #[arg(long, env = "ATLAS_MAC_BIND", default_value = "127.0.0.1:5112")]
    bind: SocketAddr,
    /// `fake` (recorded fixtures) or `real` (runs `diskutil` on this Mac).
    #[arg(long, env = "ATLAS_MAC_MODE", default_value = "fake")]
    mode: String,
    /// Display name for this Mac in the inventory.
    #[arg(long, env = "ATLAS_MAC_HOST")]
    host: Option<String>,
    /// Also report mounted disk images (tests only).
    #[arg(long, env = "ATLAS_MAC_ALLOW_DISK_IMAGES")]
    allow_disk_images: bool,
    #[arg(long, env = "ATLAS_MAC_TLS_CERT")]
    tls_cert: Option<String>,
    #[arg(long, env = "ATLAS_MAC_TLS_KEY")]
    tls_key: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();

    // The token comes from the environment or a file only (a flag would show in `ps`).
    // `ATLAS_MAC_TOKEN_FILE` keeps it out of a world-readable launchd plist.
    let token = match std::env::var("ATLAS_MAC_TOKEN_FILE").ok().filter(|f| !f.is_empty()) {
        Some(f) => Some(
            std::fs::read_to_string(&f)
                .map_err(|e| anyhow::anyhow!("ATLAS_MAC_TOKEN_FILE {f}: {e}"))?,
        ),
        None => std::env::var("ATLAS_MAC_TOKEN").ok(),
    }
    .map(|t| t.trim().to_string())
    .filter(|t| !t.is_empty());
    let tls = match (&cli.tls_cert, &cli.tls_key) {
        (Some(c), Some(k)) => Some((c.clone(), k.clone())),
        (None, None) => None,
        _ => bail!("ATLAS_MAC_TLS_CERT and ATLAS_MAC_TLS_KEY must be set together"),
    };
    if !cli.bind.ip().is_loopback() && (token.is_none() || tls.is_none()) {
        bail!(
            "refusing to listen on {}: a non-loopback bind needs ATLAS_MAC_TOKEN and ATLAS_MAC_TLS_CERT/KEY",
            cli.bind
        );
    }

    let fake = cli.mode != "real";
    let host = cli
        .host
        .unwrap_or_else(|| if fake { "mac01.zyvor.lab".into() } else { "localhost".into() });
    let settings = atlas_driver_macos::EnvSettings {
        enabled: true,
        fake,
        host,
        allow_disk_images: cli.allow_disk_images,
    };
    let app = atlas_mac_agent::router(settings.driver("bkd_macos"), token);
    tracing::info!(bind = %cli.bind, mode = %cli.mode, tls = tls.is_some(), "atlas-mac-agent listening");
    match tls {
        Some((cert, key)) => {
            let cfg = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key).await?;
            axum_server::bind_rustls(cli.bind, cfg)
                .serve(app.into_make_service())
                .await?;
        }
        None => {
            let listener = tokio::net::TcpListener::bind(cli.bind).await?;
            axum::serve(listener, app).await?;
        }
    }
    Ok(())
}
