// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! `atlas-native-mount --endpoint https://node:7400 --fs <id> /mnt/point`: mounts an atlas-native
//! filesystem (or, with `--snapshot`, one of its snapshots read-only) through FUSE. Runs in the
//! foreground until unmounted (`fusermount3 -u /mnt/point`).

use std::{path::PathBuf, process::ExitCode, sync::Arc, time::Duration};

use atlas_native::TlsIdentity;
use atlas_native_fuse::{
    client::{Client, ClientConfig},
    direct_write::DirectWrites,
    ops::{DirectReads, Ops, OpsConfig},
};
use clap::Parser;

#[derive(Parser)]
#[command(version, about = "Mount an atlas-native filesystem through FUSE")]
struct Args {
    /// Metadata node base URL; repeat (or comma-separate) for every node.
    #[arg(
        long,
        env = "ATLAS_NATIVE_ENDPOINTS",
        value_delimiter = ',',
        required = true
    )]
    endpoint: Vec<String>,
    /// Filesystem id.
    #[arg(long)]
    fs: String,
    /// Mount this snapshot of the filesystem, read-only.
    #[arg(long)]
    snapshot: Option<String>,
    mountpoint: PathBuf,
    /// File holding the API bearer token.
    #[arg(long, env = "ATLAS_NATIVE_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// PEM CA bundle for HTTPS endpoints.
    #[arg(long)]
    ca_file: Option<PathBuf>,
    /// PEM client certificate and key (one file, certificate first) for mTLS.
    #[arg(long)]
    identity_file: Option<PathBuf>,
    /// Attribute and name cache lifetime (also given to the kernel).
    #[arg(long, default_value_t = 1000)]
    ttl_ms: u64,
    /// Buffer up to this many bytes of sequential writes per file before sending them.
    #[arg(long, default_value_t = 4 << 20)]
    writeback_bytes: usize,
    /// Full write-back runs sent at once in the background (0 sends them in the write call). A
    /// failed one is reported by the next write to the file, fsync or close.
    #[arg(long, default_value_t = 4)]
    writeback_parallel: usize,
    /// Fetch at least this much per read and serve following reads from it (0 disables).
    #[arg(long, default_value_t = 4 << 20)]
    readahead_bytes: usize,
    /// Largest single request; keep at or below the nodes' `max_request_bytes`.
    #[arg(long, default_value_t = 8 << 20)]
    max_io_bytes: usize,
    /// Keep retrying through elections and unreachable nodes for this long before failing a call.
    #[arg(long, default_value_t = 30)]
    retry_secs: u64,
    /// Mount read-only.
    #[arg(long)]
    read_only: bool,
    /// Let other users access the mount (needs `user_allow_other` in /etc/fuse.conf unless root).
    #[arg(long)]
    allow_other: bool,
    /// Read file data straight from the data nodes (verified by checksum, falling back to the
    /// leader on any failure). Needs network access to the data nodes; when they require mTLS,
    /// `--identity-file` must hold a certificate signed by the cluster CA in `--ca-file`.
    #[arg(long)]
    direct_reads: bool,
    /// Write whole extents straight to the data nodes, with a short-lived write grant the
    /// metadata leader leases per write, instead of sending the bytes through the leader (which
    /// then only allocates and commits). Unaligned heads and tails, and any write that fails
    /// this way, still go through the leader. Needs network access to the data nodes and a
    /// cluster configured with `grant_key_file`; without one every attempt is refused and the
    /// mount keeps using the leader. Off by default.
    #[arg(long)]
    direct_writes: bool,
    /// With `--direct-reads`: read full copies on this data-node host (its failure-domain
    /// `host`) first, e.g. the host this client runs on after `POST /v1/fs/{fs}/locality/pin`.
    #[arg(long, requires = "direct_reads")]
    prefer_host: Option<String>,
    /// Kernel request worker threads (Linux; each gets its own /dev/fuse fd). At most one fewer
    /// blocking lock requests (`F_SETLKW`) wait at once; more fail with ENOLCK.
    #[arg(long, default_value_t = 4)]
    fuse_threads: usize,
    /// Lease of the session holding this mount's file locks: if the mount stops renewing it
    /// (crash, partition), the cluster releases its locks this long after the last renewal.
    #[arg(long, default_value_t = 15_000)]
    session_ttl_ms: u64,
    /// Cache attributes and names under cache leases from the cluster instead of for `--ttl-ms`:
    /// a change by another client recalls them first, so nothing cached is stale.
    #[arg(long)]
    cache_leases: bool,
    /// Enforce POSIX ACLs (`setfacl`/`getfacl`): the kernel checks them on every access, at the
    /// cost of one ACL lookup per inode it hasn't cached.
    #[arg(long)]
    acl: bool,
}

fn read(path: &Option<PathBuf>, what: &str) -> Result<Option<Vec<u8>>, String> {
    path.as_ref()
        .map(|p| std::fs::read(p).map_err(|e| format!("{what} {}: {e}", p.display())))
        .transpose()
}

fn ops(args: &Args) -> Result<Ops, String> {
    let mut cfg = ClientConfig::new(args.endpoint.clone());
    cfg.token = read(&args.token_file, "token file")?
        .map(|t| String::from_utf8_lossy(&t).trim().to_string());
    cfg.ca_pem = read(&args.ca_file, "CA file")?;
    cfg.identity_pem = read(&args.identity_file, "identity file")?;
    cfg.retry_for = Duration::from_secs(args.retry_secs);
    let identity = if args.direct_reads || args.direct_writes {
        match (&cfg.ca_pem, &cfg.identity_pem) {
            (Some(ca), Some(id)) => Some(Arc::new(
                TlsIdentity::from_pem(ca, id, id).map_err(|e| format!("identity: {e}"))?,
            )),
            _ => None,
        }
    } else {
        None
    };
    let direct_reads = args.direct_reads.then(|| DirectReads {
        identity: identity.clone(),
        timeout: cfg.request_timeout,
        prefer_host: args.prefer_host.clone(),
    });
    let direct_writes = args.direct_writes.then(|| DirectWrites {
        identity: identity.clone(),
        timeout: cfg.request_timeout,
    });
    let client = Client::new(cfg).map_err(|e| e.to_string())?;
    let fs = match &args.snapshot {
        Some(s) => format!("{}@{s}", args.fs),
        None => args.fs.clone(),
    };
    let ops = Ops::new(
        client,
        fs,
        OpsConfig {
            ttl: Duration::from_millis(args.ttl_ms),
            writeback_bytes: args.writeback_bytes,
            writeback_parallel: args.writeback_parallel,
            readahead_bytes: args.readahead_bytes,
            max_io_bytes: args.max_io_bytes,
            direct_reads,
            direct_writes,
            session_ttl: Duration::from_millis(args.session_ttl_ms),
            lock_waiters: args.fuse_threads.saturating_sub(1),
            cache_leases: args.cache_leases,
        },
    );
    // Fail fast on a wrong endpoint, token or filesystem id instead of at first access.
    ops.getattr(atlas_native::ROOT_INO)
        .map_err(|e| format!("filesystem {} is not reachable (errno {e})", ops.fs()))?;
    Ok(ops)
}

#[cfg(all(feature = "fuse", target_os = "linux"))]
fn mount(args: &Args, ops: Ops) -> Result<(), String> {
    use fuser::{Config, MountOption, SessionACL};
    let mut cfg = Config::default();
    cfg.mount_options = vec![
        MountOption::FSName(format!("atlas-native:{}", ops.fs())),
        MountOption::Subtype("atlas".into()),
        MountOption::DefaultPermissions,
    ];
    if args.read_only || ops.read_only() {
        cfg.mount_options.push(MountOption::RO);
    }
    if args.allow_other {
        cfg.acl = SessionACL::All;
    }
    cfg.n_threads = Some(args.fuse_threads.max(1));
    cfg.clone_fd = args.fuse_threads > 1;
    let fs = atlas_native_fuse::fuse::AtlasFs::new(ops).with_posix_acl(args.acl);
    fuser::mount(fs, &args.mountpoint, &cfg).map_err(|e| format!("mount: {e}"))
}

#[cfg(not(all(feature = "fuse", target_os = "linux")))]
fn mount(_args: &Args, _ops: Ops) -> Result<(), String> {
    Err("this build has no FUSE support; rebuild on Linux with `--features fuse`".into())
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "atlas_native_fuse=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    // reqwest's rustls backend can't pick between ring and aws-lc-rs when both are compiled in.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = Args::parse();
    let result = ops(&args).and_then(|ops| mount(&args, ops));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("atlas-native-mount: {e}");
            ExitCode::FAILURE
        }
    }
}
