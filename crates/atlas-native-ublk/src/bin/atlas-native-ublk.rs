// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `atlas-native-ublk --endpoint https://node:7400 --volume <id>`: attaches an atlas-native block
//! volume as `/dev/ublkbN`. Runs in the foreground until interrupted. `--check` only looks the
//! volume up and prints what the device would be, on any OS and without privileges.

use std::{path::PathBuf, process::ExitCode, time::Duration};

use atlas_native_fuse::client::{Client, ClientConfig};
use atlas_native_ublk::{
    params::{check_io_bytes, device_params},
    volume::{Volume, VolumeConfig},
};
use clap::Parser;

#[derive(Parser)]
#[command(
    version,
    about = "Attach an atlas-native block volume as a Linux block device (ublk)"
)]
struct Args {
    /// Metadata node base URL; repeat (or comma-separate) for every node.
    #[arg(
        long,
        env = "ATLAS_NATIVE_ENDPOINTS",
        value_delimiter = ',',
        required = true
    )]
    endpoint: Vec<String>,
    /// Volume id. Attach a volume in one place at a time: nothing in the cluster prevents a
    /// second writer.
    #[arg(long)]
    volume: String,
    /// File holding the API bearer token.
    #[arg(long, env = "ATLAS_NATIVE_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// PEM CA bundle for HTTPS endpoints.
    #[arg(long)]
    ca_file: Option<PathBuf>,
    /// PEM client certificate and key (one file, certificate first) for mTLS.
    #[arg(long)]
    identity_file: Option<PathBuf>,
    /// Attach read-only.
    #[arg(long)]
    read_only: bool,
    /// Logical block size: 512, 1024, 2048 or 4096.
    #[arg(long, default_value_t = 4096)]
    block_size: u32,
    /// Hardware queues, each served by its own thread.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=64))]
    queues: u16,
    /// Requests in flight per queue. Each has an I/O thread and an `--io-bytes` buffer.
    #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u16).range(1..=1024))]
    queue_depth: u16,
    /// Largest read or write the device accepts (multiple of 4096, at most 32 MiB).
    #[arg(long, default_value_t = 4 << 20)]
    io_bytes: u32,
    /// Largest single cluster request; keep at or below the nodes' `max_request_bytes`.
    #[arg(long, default_value_t = 8 << 20)]
    max_request_bytes: u64,
    /// Keep retrying a request this long before failing it with an I/O error; 0 retries for
    /// ever, so I/O hangs (instead of failing) while the cluster is unavailable.
    #[arg(long, default_value_t = 60)]
    io_timeout_secs: u64,
    /// Give up on one unanswered cluster request after this long, and try again.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    request_timeout_secs: u64,
    /// After a write request went unanswered, hold the write this long so the abandoned request
    /// cannot land after a newer write. See docs/NATIVE_UBLK.md.
    #[arg(long, default_value_t = 15)]
    settle_secs: u64,
    /// Device number to use (`/dev/ublkbN`); -1 takes the first free one.
    #[arg(long, default_value_t = -1, allow_hyphen_values = true,
          value_parser = clap::value_parser!(i32).range(-1..))]
    device_id: i32,
    /// Stay in the caller's mount namespace. Mounting the device where its server can see the
    /// mount can deadlock when the server exits.
    #[arg(long)]
    share_mount_namespace: bool,
    /// Only check that the volume can be attached, print the device it would be, and exit.
    #[arg(long)]
    check: bool,
}

fn read(path: &Option<PathBuf>, what: &str) -> Result<Option<Vec<u8>>, String> {
    path.as_ref()
        .map(|p| std::fs::read(p).map_err(|e| format!("{what} {}: {e}", p.display())))
        .transpose()
}

fn volume_config(args: &Args) -> VolumeConfig {
    VolumeConfig {
        block_bytes: args.block_size,
        max_request_bytes: args.max_request_bytes,
        io_timeout: (args.io_timeout_secs > 0).then(|| Duration::from_secs(args.io_timeout_secs)),
        settle: Duration::from_secs(args.settle_secs),
        read_only: args.read_only,
    }
}

fn open(args: &Args) -> Result<Volume, String> {
    check_io_bytes(args.io_bytes)?;
    let mut cfg = ClientConfig::new(args.endpoint.clone());
    cfg.token = read(&args.token_file, "token file")?
        .map(|t| String::from_utf8_lossy(&t).trim().to_string());
    cfg.ca_pem = read(&args.ca_file, "CA file")?;
    cfg.identity_pem = read(&args.identity_file, "identity file")?;
    cfg.request_timeout = Duration::from_secs(args.request_timeout_secs);
    let client = Client::new(cfg).map_err(|e| e.to_string())?;
    Volume::open(client, &args.volume, volume_config(args))
}

fn describe(args: &Args, volume: &Volume) -> String {
    let g = volume.geometry();
    let p = device_params(g, args.io_bytes, volume.read_only());
    format!(
        "volume {}: {} bytes, {}-byte blocks, {}-byte extents, requests up to {} bytes{}",
        volume.id(),
        g.size_bytes,
        g.block_bytes,
        g.extent_bytes,
        u64::from(p.max_sectors) * 512,
        if p.read_only { ", read-only" } else { "" }
    )
}

#[cfg(all(feature = "ublk", target_os = "linux"))]
fn run(args: &Args) -> Result<(), String> {
    use atlas_native_ublk::ublk::{DeviceOptions, Process};
    if args.check {
        println!("{}", describe(args, &open(args)?));
        return Ok(());
    }
    // Before anything starts a thread.
    let process = Process::prepare(args.share_mount_namespace)?;
    let volume = std::sync::Arc::new(open(args)?);
    tracing::info!("{}", describe(args, &volume));
    process.serve(
        volume,
        DeviceOptions {
            id: args.device_id,
            queues: args.queues,
            depth: args.queue_depth,
            io_bytes: args.io_bytes,
        },
        // The one line on stdout, for scripts.
        |path| println!("device {path}"),
    )
}

#[cfg(not(all(feature = "ublk", target_os = "linux")))]
fn run(args: &Args) -> Result<(), String> {
    println!("{}", describe(args, &open(args)?));
    if args.check {
        return Ok(());
    }
    Err("this build has no ublk support; rebuild on Linux with `--features ublk`".into())
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "atlas_native_ublk=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    // reqwest's rustls backend can't pick between ring and aws-lc-rs when both are compiled in.
    let _ = rustls::crypto::ring::default_provider().install_default();
    match run(&Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("atlas-native-ublk: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn args(extra: &[&str]) -> Result<Args, clap::Error> {
        let mut v = vec![
            "atlas-native-ublk",
            "--endpoint",
            "http://x:1,http://y:1",
            "--volume",
            "v",
        ];
        v.extend_from_slice(extra);
        Args::try_parse_from(v)
    }

    #[test]
    fn defaults() {
        Args::command().debug_assert();
        let a = args(&[]).unwrap();
        assert_eq!(a.endpoint.len(), 2);
        assert_eq!(a.device_id, -1);
        let c = volume_config(&a);
        assert_eq!(c.io_timeout, Some(Duration::from_secs(60)));
        assert_eq!(c.block_bytes, 4096);
    }

    #[test]
    fn a_zero_io_timeout_never_gives_up() {
        let a = args(&["--io-timeout-secs", "0", "--device-id", "-1"]).unwrap();
        assert_eq!(volume_config(&a).io_timeout, None);
    }

    #[test]
    fn out_of_range_values_are_refused() {
        for bad in [
            ["--queues", "0"],
            ["--queue-depth", "0"],
            ["--device-id", "-2"],
            ["--request-timeout-secs", "0"],
        ] {
            assert!(args(&bad).is_err(), "{bad:?}");
        }
    }
}
