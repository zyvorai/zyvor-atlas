// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `atlas-native-replicate --source https://a:7400 --target https://b:7400 --fs data`: keeps a
//! read-only replica of a filesystem on another native cluster, one snapshot increment every
//! `--interval-secs`. Stateless: a restart continues from the replica's own state.

use std::{
    path::PathBuf,
    process::ExitCode,
    thread::sleep,
    time::{Duration, Instant},
};

use atlas_native_fuse::{
    client::{Client, ClientConfig},
    replicate::{ReplicateConfig, Replicator},
    supervise::{self, stopping},
};
use clap::Parser;

#[derive(Parser)]
#[command(
    version,
    about = "Replicate an atlas-native filesystem to another cluster"
)]
struct Args {
    /// Source metadata node base URL; repeat (or comma-separate) for every node.
    #[arg(long, value_delimiter = ',', required = true)]
    source: Vec<String>,
    #[arg(long)]
    source_token_file: Option<PathBuf>,
    /// PEM CA bundle for HTTPS source endpoints.
    #[arg(long)]
    source_ca_file: Option<PathBuf>,
    /// Target metadata node base URL; repeat (or comma-separate) for every node.
    #[arg(long, value_delimiter = ',', required = true)]
    target: Vec<String>,
    #[arg(long)]
    target_token_file: Option<PathBuf>,
    #[arg(long)]
    target_ca_file: Option<PathBuf>,
    /// Source filesystem id.
    #[arg(long)]
    fs: String,
    /// Replica filesystem id on the target (default: the source's id).
    #[arg(long)]
    target_fs: Option<String>,
    /// Seconds between rounds (from the start of one to the start of the next).
    #[arg(long, default_value_t = 60)]
    interval_secs: u64,
    /// Run one round and exit.
    #[arg(long)]
    once: bool,
    /// Replicated snapshots kept on the target.
    #[arg(long, default_value_t = 24)]
    keep: usize,
    /// Id prefix of the snapshots the replicator takes and prunes.
    #[arg(long, default_value = "repl-")]
    prefix: String,
    /// Files copied at once.
    #[arg(long, default_value_t = 4)]
    parallel: usize,
    /// Largest data request; keep at or below both clusters' `max_request_bytes`.
    #[arg(long, default_value_t = 4 << 20)]
    max_io_bytes: usize,
    /// Keep retrying through elections and unreachable nodes for this long before failing a round.
    #[arg(long, default_value_t = 60)]
    retry_secs: u64,
}

fn read(path: &Option<PathBuf>, what: &str) -> Result<Option<Vec<u8>>, String> {
    path.as_ref()
        .map(|p| std::fs::read(p).map_err(|e| format!("{what} {}: {e}", p.display())))
        .transpose()
}

fn client(
    endpoints: &[String],
    token: &Option<PathBuf>,
    ca: &Option<PathBuf>,
    retry: Duration,
) -> Result<Client, String> {
    let mut cfg = ClientConfig::new(endpoints.to_vec());
    cfg.token = read(token, "token file")?.map(|t| String::from_utf8_lossy(&t).trim().to_string());
    cfg.ca_pem = read(ca, "CA file")?;
    cfg.retry_for = retry;
    Client::new(cfg).map_err(|e| e.to_string())
}

fn replicator(args: &Args) -> Result<Replicator, String> {
    let ok_id = |s: &str| {
        (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if args.prefix.len() > 32
        || !args
            .prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err("--prefix must be at most 32 characters of [A-Za-z0-9-]".into());
    }
    let target_fs = args.target_fs.clone().unwrap_or_else(|| args.fs.clone());
    if !ok_id(&args.fs) || !ok_id(&target_fs) {
        return Err("filesystem ids are 1-64 characters of [A-Za-z0-9-]".into());
    }
    let retry = Duration::from_secs(args.retry_secs);
    let source = client(
        &args.source,
        &args.source_token_file,
        &args.source_ca_file,
        retry,
    )?;
    let target = client(
        &args.target,
        &args.target_token_file,
        &args.target_ca_file,
        retry,
    )?;
    let mut cfg = ReplicateConfig::new(&args.fs, target_fs);
    cfg.prefix.clone_from(&args.prefix);
    cfg.keep = args.keep.max(1);
    cfg.parallel = args.parallel.max(1);
    cfg.max_io_bytes = args.max_io_bytes;
    Ok(Replicator::new(source, target, cfg))
}

fn round(r: &Replicator) -> bool {
    let start = Instant::now();
    match r.run_once() {
        Ok(done) => {
            tracing::info!(
                snapshot = %done.snapshot,
                from = done.from.as_deref().unwrap_or("-"),
                resumed = done.resumed,
                inodes = done.inodes,
                removed = done.removed,
                bytes = done.bytes,
                secs = start.elapsed().as_secs_f64(),
                "replicated"
            );
            true
        }
        Err(e) => {
            tracing::error!(error = %e, "replication round failed");
            false
        }
    }
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "atlas_native_replicate=info,atlas_native_fuse=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    // reqwest's rustls backend can't pick between ring and aws-lc-rs when both are compiled in.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = Args::parse();
    let r = match replicator(&args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("atlas-native-replicate: {e}");
            return ExitCode::FAILURE;
        }
    };
    if args.once {
        return if round(&r) {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    supervise::handle_signals();
    let interval = Duration::from_secs(args.interval_secs.max(1));
    tracing::info!(
        fs = %r.config().fs,
        target_fs = %r.config().target_fs,
        interval_secs = interval.as_secs(),
        "replicating"
    );
    while !stopping() {
        let next = Instant::now() + interval;
        round(&r);
        while !stopping() && Instant::now() < next {
            sleep(Duration::from_millis(250));
        }
    }
    tracing::info!("stopping");
    ExitCode::SUCCESS
}
