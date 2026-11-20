// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `atlas-native-csi --controller|--node --endpoint https://atlas-native:7400`: the CSI plugin
//! for atlas-native filesystems, serving on `--csi-endpoint` (a Unix socket).

use std::{path::PathBuf, process::ExitCode, time::Duration};

use atlas_native_csi::{
    controller::ControllerService,
    identity::IdentityService,
    node::{NodeConfig, NodeService},
    serve, socket_path,
};
use atlas_native_fuse::client::{Client, ClientConfig};
use clap::Parser;

#[derive(Parser)]
#[command(version, about = "CSI driver for atlas-native filesystems")]
struct Args {
    /// CSI socket to serve on.
    #[arg(long, env = "CSI_ENDPOINT", default_value = "unix:///csi/csi.sock")]
    csi_endpoint: String,
    /// Serve the controller service (provisioning and snapshots).
    #[arg(long)]
    controller: bool,
    /// Serve the node service (publishing volumes on this node).
    #[arg(long)]
    node: bool,
    /// Metadata node base URL; repeat (or comma-separate) for every node.
    #[arg(
        long,
        env = "ATLAS_NATIVE_ENDPOINTS",
        value_delimiter = ',',
        required = true
    )]
    endpoint: Vec<String>,
    /// File holding the API bearer token.
    #[arg(long, env = "ATLAS_NATIVE_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// PEM CA bundle for HTTPS endpoints.
    #[arg(long)]
    ca_file: Option<PathBuf>,
    /// PEM client certificate plus key, for nodes that require mTLS.
    #[arg(long)]
    identity_file: Option<PathBuf>,
    /// This node's name (required with `--node`; the Kubernetes node name).
    #[arg(long, env = "NODE_ID")]
    node_id: Option<String>,
    #[arg(long, default_value = "atlas-native-mount")]
    mount_binary: PathBuf,
    /// Seconds a publish waits for its mount to appear.
    #[arg(long, default_value_t = 30)]
    mount_timeout_secs: u64,
}

fn read(path: &Option<PathBuf>, what: &str) -> Result<Option<Vec<u8>>, String> {
    path.as_ref()
        .map(|p| std::fs::read(p).map_err(|e| format!("{what} {}: {e}", p.display())))
        .transpose()
}

fn controller(args: &Args) -> Result<ControllerService, String> {
    let mut cfg = ClientConfig::new(args.endpoint.clone());
    cfg.token = read(&args.token_file, "token file")?
        .map(|t| String::from_utf8_lossy(&t).trim().to_string());
    cfg.ca_pem = read(&args.ca_file, "CA file")?;
    cfg.identity_pem = read(&args.identity_file, "identity file")?;
    // Below the sidecars' default 10 s call timeout; they retry, and every call is idempotent.
    cfg.retry_for = Duration::from_secs(8);
    cfg.request_timeout = Duration::from_secs(8);
    Ok(ControllerService::new(
        Client::new(cfg).map_err(|e| e.to_string())?,
    ))
}

fn node(args: &Args) -> Result<NodeService, String> {
    let node_id = args
        .node_id
        .clone()
        .filter(|n| !n.is_empty())
        .ok_or("--node needs --node-id (or NODE_ID)")?;
    let mut mount_args = vec!["--endpoint".to_string(), args.endpoint.join(",")];
    for (flag, path) in [
        ("--token-file", &args.token_file),
        ("--ca-file", &args.ca_file),
        ("--identity-file", &args.identity_file),
    ] {
        if let Some(p) = path {
            mount_args.push(flag.into());
            mount_args.push(p.to_string_lossy().into_owned());
        }
    }
    Ok(NodeService::new(NodeConfig {
        node_id,
        mount_binary: args.mount_binary.clone(),
        mount_args,
        mount_timeout: Duration::from_secs(args.mount_timeout_secs),
    }))
}

async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

fn run(args: Args) -> Result<(), String> {
    if !args.controller && !args.node {
        return Err("pass --controller, --node, or both".into());
    }
    // The blocking HTTP client is built (and dropped) outside the async runtime.
    let controller = args.controller.then(|| controller(&args)).transpose()?;
    let node = args.node.then(|| node(&args)).transpose()?;
    let path = PathBuf::from(socket_path(&args.csi_endpoint));
    tracing::info!(
        socket = %path.display(),
        controller = args.controller,
        node = args.node,
        "serving CSI"
    );
    let identity = IdentityService {
        controller: args.controller,
    };
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("runtime: {e}"))?;
    rt.block_on(serve(&path, identity, controller, node, shutdown()))
        .map_err(|e| format!("CSI server: {e}"))
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "atlas_native_csi=info".into()),
        )
        .init();
    // reqwest's rustls backend can't pick between ring and aws-lc-rs when both are compiled in.
    let _ = rustls::crypto::ring::default_provider().install_default();
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}
