// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `atlas-native-nfs --endpoint https://node:7400 --exports-file exports.json`: the NFS gateway.
//! Mounts every exported filesystem with `atlas-native-mount` under `--root`, writes an
//! NFS-Ganesha configuration exporting the mounts (FSAL_VFS) and runs `ganesha.nfsd` (and, for
//! NFSv3, `rpcbind` and the `rpc.statd` its NLM locks need). On SIGTERM it stops Ganesha, then unmounts, so buffered writes reach the
//! cluster; if any process exits, it shuts the rest down and fails.

use std::{
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    thread::sleep,
    time::{Duration, Instant},
};

use atlas_native_fuse::{
    nfs::{self, Export},
    supervise::{self, mounted, stopping, Proc},
};
use clap::Parser;

#[derive(Parser)]
#[command(
    version,
    about = "Serve atlas-native filesystems over NFS through NFS-Ganesha"
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
    /// File holding the API bearer token.
    #[arg(long, env = "ATLAS_NATIVE_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// JSON list of exports: `[{"fs": "...", "clients": ["10.0.0.0/8"], "pseudo": "/x",
    /// "access": "rw"|"ro", "squash": "root"|"none"|"all"}]`.
    #[arg(long)]
    exports_file: PathBuf,
    /// Serve NFSv3 (with NLM locks and rpcbind) as well as NFSv4.1/4.2.
    #[arg(long)]
    nfs_v3: bool,
    /// Directory the filesystems are mounted under, one subdirectory each.
    #[arg(long, default_value = "/export")]
    root: PathBuf,
    /// Where to write the Ganesha configuration.
    #[arg(long, default_value = "/run/atlas-native-nfs/ganesha.conf")]
    config: PathBuf,
    /// Extra argument for every `atlas-native-mount` (repeat), e.g. `--mount-arg=--cache-leases`.
    #[arg(long = "mount-arg", allow_hyphen_values = true)]
    mount_args: Vec<String>,
    /// How long to wait for the mounts to come up.
    #[arg(long, default_value_t = 60)]
    mount_timeout_secs: u64,
    #[arg(long, default_value = "atlas-native-mount")]
    mount_bin: String,
    #[arg(long, default_value = "ganesha.nfsd")]
    ganesha_bin: String,
    #[arg(long, default_value = "rpcbind")]
    rpcbind_bin: String,
    #[arg(long, default_value = "rpc.statd")]
    statd_bin: String,
}

fn exports(args: &Args) -> Result<Vec<Export>, String> {
    let raw = std::fs::read(&args.exports_file)
        .map_err(|e| format!("exports file {}: {e}", args.exports_file.display()))?;
    let exports: Vec<Export> =
        serde_json::from_slice(&raw).map_err(|e| format!("exports file: {e}"))?;
    nfs::validate(&exports)?;
    Ok(exports)
}

fn run(args: &Args, exports: &[Export], procs: &mut Vec<Proc>) -> Result<(), String> {
    for e in exports {
        let dir = args.root.join(&e.fs);
        std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        if mounted(&dir) {
            return Err(format!("{} is already mounted", dir.display()));
        }
        let mut cmd = Command::new(&args.mount_bin);
        cmd.arg("--endpoint")
            .arg(args.endpoint.join(","))
            .arg("--fs")
            .arg(&e.fs)
            .arg("--allow-other");
        if let Some(t) = &args.token_file {
            cmd.arg("--token-file").arg(t);
        }
        if e.access == nfs::Access::Ro {
            cmd.arg("--read-only");
        }
        cmd.args(&args.mount_args).arg(&dir);
        procs.push(Proc::spawn(format!("mount of {}", e.fs), &mut cmd)?);
    }
    let until = Instant::now() + Duration::from_secs(args.mount_timeout_secs);
    while !exports.iter().all(|e| mounted(&args.root.join(&e.fs))) {
        if let Some(why) = procs.iter_mut().find_map(Proc::exited) {
            return Err(why);
        }
        if stopping() {
            return Ok(());
        }
        if Instant::now() > until {
            return Err("the mounts did not come up in time".into());
        }
        sleep(Duration::from_millis(200));
    }
    tracing::info!(exports = exports.len(), "filesystems mounted");

    if args.nfs_v3 {
        procs.push(Proc::spawn(
            "rpcbind",
            Command::new(&args.rpcbind_bin).args(["-f", "-w"]),
        )?);
        let until = Instant::now() + Duration::from_secs(10);
        while !Path::new("/run/rpcbind.sock").exists() {
            if let Some(why) = procs.iter_mut().find_map(Proc::exited) {
                return Err(why);
            }
            if Instant::now() > until {
                return Err("rpcbind did not start".into());
            }
            sleep(Duration::from_millis(100));
        }
        // No reboot notifications: the gateway keeps no lock state across restarts to reclaim.
        procs.push(Proc::spawn(
            "rpc.statd",
            Command::new(&args.statd_bin).args(["-F", "--no-notify"]),
        )?);
        sleep(Duration::from_millis(500));
    }
    if let Some(dir) = args.config.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    for dir in ["/var/run/ganesha", "/var/lib/nfs/ganesha"] {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&args.config, nfs::render(exports, &args.root, args.nfs_v3))
        .map_err(|e| format!("{}: {e}", args.config.display()))?;
    procs.push(Proc::spawn(
        "ganesha.nfsd",
        Command::new(&args.ganesha_bin)
            .args(["-F", "-L", "STDOUT", "-f"])
            .arg(&args.config),
    )?);
    tracing::info!(v3 = args.nfs_v3, "serving NFS");

    while !stopping() {
        if let Some(why) = procs.iter_mut().find_map(Proc::exited) {
            return Err(why);
        }
        sleep(Duration::from_millis(250));
    }
    tracing::info!("stopping");
    Ok(())
}

/// Stops Ganesha and rpcbind, then unmounts each filesystem (which sends its buffered writes).
fn shutdown(args: &Args, exports: &[Export], procs: &mut [Proc]) {
    for p in procs
        .iter_mut()
        .filter(|p| !p.name.starts_with("mount of "))
    {
        p.stop(Duration::from_secs(20));
    }
    for e in exports {
        supervise::unmount(&args.root.join(&e.fs));
    }
    for p in procs.iter_mut() {
        p.stop(Duration::from_secs(30));
    }
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "atlas_native_nfs=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    supervise::handle_signals();
    let exports = match exports(&args) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("atlas-native-nfs: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut procs = Vec::new();
    let result = run(&args, &exports, &mut procs);
    shutdown(&args, &exports, &mut procs);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("atlas-native-nfs: {e}");
            ExitCode::FAILURE
        }
    }
}
