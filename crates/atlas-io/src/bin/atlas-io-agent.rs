// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Node agent. Fake mode is the default so `make run` / CI never need CAP_BPF.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use atlas_io::devmap::DeviceMap;
use atlas_io::source::{self, FakeSource, LiveSource};
use atlas_io::Collector;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "atlas-io-agent",
    about = "Atlas observe-first storage I/O sensor"
)]
struct Cli {
    /// Bind address.
    #[arg(long, env = "ATLAS_IO_BIND", default_value = "127.0.0.1:5111")]
    bind: SocketAddr,
    /// `fake` (scripted events) or `live` (block-layer BPF; needs the `bpf` build and
    /// CAP_BPF + CAP_PERFMON, else it reports the programs missing).
    #[arg(long, env = "ATLAS_IO_MODE", default_value = "fake")]
    mode: String,
    /// Atlas Native binary names whose block I/O is aggregated kernel-side (live mode). Matched
    /// against /proc/<pid>/comm, so a containerised agent needs hostPID.
    #[arg(
        long = "native-process",
        env = "ATLAS_IO_NATIVE_PROCESSES",
        value_delimiter = ',',
        default_value = atlas_io::DEFAULT_NATIVE_PROCESSES
    )]
    native_processes: Vec<String>,
    /// bpffs directory to pin the native maps in (live mode); empty disables pinning. Container
    /// runtimes' default AppArmor profiles refuse writes under /sys/fs/bpf, so mount the host
    /// bpffs elsewhere (e.g. /host-bpf) and point this there.
    #[arg(long, env = "ATLAS_IO_NATIVE_PIN_DIR", default_value = atlas_io::NATIVE_PIN_DIR)]
    native_pin_dir: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let cli = Cli::parse();
    let collector = match cli.mode.as_str() {
        "live" => {
            let src = source::live(
                &cli.native_processes,
                (!cli.native_pin_dir.is_empty()).then(|| Path::new(&cli.native_pin_dir)),
            )
            .unwrap_or_else(|e| {
                tracing::warn!(error = %format!("{e:#}"), "BPF attach failed; reporting programs missing");
                Box::new(LiveSource)
            });
            Collector::new(src, DeviceMap::from_sysfs())
        }
        _ => Collector::new(Box::new(FakeSource::demo()), DeviceMap::lab()),
    };
    collector.poll();
    let poller = collector.clone();
    let poll_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            poller.poll();
        }
    });
    tracing::info!(%cli.bind, mode = %cli.mode, seen = collector.seen(), "atlas-io-agent listening");
    // Bearer token for the whole API except /health, from the environment only (a flag would leak
    // into `ps`). Unset keeps today's open behaviour — fine on loopback, not on a cluster Service.
    let token = std::env::var("ATLAS_IO_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    if token.is_none() && !cli.bind.ip().is_loopback() {
        tracing::warn!(
            "ATLAS_IO_TOKEN is not set and the agent is bound to a non-loopback address: its API is unauthenticated"
        );
    }
    let app = atlas_io::http::router_with_token(collector, token);
    let listener = tokio::net::TcpListener::bind(cli.bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    // Dropping the last collector handle drops the source: the BPF programs detach and the
    // native map pins are removed.
    poll_task.abort();
    let _ = poll_task.await;
    tracing::info!("atlas-io-agent stopped");
    Ok(())
}

/// SIGINT or SIGTERM (pod deletion, `podman stop`).
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
}
