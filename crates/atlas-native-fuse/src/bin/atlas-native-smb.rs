// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `atlas-native-smb --endpoint https://node:7400 --shares-file shares.json --users-file
//! users.json`: the SMB gateway. Mounts each share's filesystem with `atlas-native-mount` under
//! `--root`, writes a Samba configuration sharing the mounts, creates a Unix account and Samba
//! password for every user, and runs `smbd`. On SIGTERM it stops `smbd`, then unmounts, so
//! buffered writes reach the cluster; if any process exits, it shuts the rest down and fails.

use std::{
    io::Write,
    path::PathBuf,
    process::{Command, ExitCode, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

use atlas_native_fuse::{
    smb::{self, Access, Encrypt, Share, User},
    supervise::{self, mounted, stopping, Proc},
};
use clap::Parser;

#[derive(Parser)]
#[command(
    version,
    about = "Serve atlas-native filesystems over SMB through Samba"
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
    /// JSON list of shares: `[{"name": "team", "fs": "...", "access": "rw"|"ro", "users":
    /// ["alice"], "read_only_users": [], "clients": ["10.0.0.0/8"], "browseable": true}]`.
    #[arg(long)]
    shares_file: PathBuf,
    /// JSON list of users: `[{"user": "alice", "password": "...", "uid": 2001, "gid": 2000}]`.
    #[arg(long)]
    users_file: PathBuf,
    /// SMB 3 encryption: desired, required or off.
    #[arg(long, default_value = "desired", value_parser = Encrypt::parse)]
    encrypt: Encrypt,
    /// Directory the shares are mounted under, one subdirectory each.
    #[arg(long, default_value = "/export")]
    root: PathBuf,
    /// Where to write the Samba configuration.
    #[arg(long, default_value = "/etc/samba/smb.conf")]
    config: PathBuf,
    /// Extra argument for every `atlas-native-mount` (repeat), e.g. `--mount-arg=--cache-leases`.
    #[arg(long = "mount-arg", allow_hyphen_values = true)]
    mount_args: Vec<String>,
    /// How long to wait for the mounts to come up.
    #[arg(long, default_value_t = 60)]
    mount_timeout_secs: u64,
    #[arg(long, default_value = "atlas-native-mount")]
    mount_bin: String,
    #[arg(long, default_value = "smbd")]
    smbd_bin: String,
}

fn load(args: &Args) -> Result<(Vec<Share>, Vec<User>), String> {
    let raw = std::fs::read(&args.shares_file)
        .map_err(|e| format!("shares file {}: {e}", args.shares_file.display()))?;
    let shares: Vec<Share> =
        serde_json::from_slice(&raw).map_err(|e| format!("shares file: {e}"))?;
    let raw = std::fs::read(&args.users_file)
        .map_err(|e| format!("users file {}: {e}", args.users_file.display()))?;
    // The parse error never quotes the file: it holds passwords.
    let users: Vec<User> = serde_json::from_slice(&raw).map_err(|e| {
        format!(
            "users file is not a valid list of users (line {})",
            e.line()
        )
    })?;
    smb::validate_users(&users)?;
    smb::validate_shares(&shares, &users)?;
    Ok((shares, users))
}

/// `(name, id)` of every entry of `/etc/passwd` or `/etc/group`.
fn entries(path: &str) -> Result<Vec<(String, u32)>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    Ok(text
        .lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            let id = f.nth(1)?.parse().ok()?;
            Some((name.to_string(), id))
        })
        .collect())
}

fn command(name: &str, cmd: &mut Command) -> Result<(), String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{name}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{name} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Creates the Unix account (and its group) for `u` unless it exists with the same ids, then
/// sets its Samba password.
fn ensure_account(args: &Args, u: &User) -> Result<(), String> {
    if !entries("/etc/group")?.iter().any(|(_, gid)| *gid == u.gid) {
        command(
            "groupadd",
            Command::new("groupadd")
                .arg("-g")
                .arg(u.gid.to_string())
                .arg(format!("smb{}", u.gid)),
        )?;
    }
    let passwd = entries("/etc/passwd")?;
    match passwd.iter().find(|(name, _)| *name == u.user) {
        Some((_, uid)) if *uid == u.uid => {}
        Some((_, uid)) => {
            return Err(format!(
                "user {} already exists in the gateway image with uid {uid}",
                u.user
            ))
        }
        None => {
            if let Some((name, _)) = passwd.iter().find(|(_, uid)| *uid == u.uid) {
                return Err(format!(
                    "user {}: uid {} belongs to {name} in the gateway image",
                    u.user, u.uid
                ));
            }
            command(
                "useradd",
                Command::new("useradd")
                    .args(["-M", "-N", "-d", "/nonexistent", "-s", "/usr/sbin/nologin"])
                    .arg("-u")
                    .arg(u.uid.to_string())
                    .arg("-g")
                    .arg(u.gid.to_string())
                    .arg(&u.user),
            )?;
        }
    }
    // `-s` reads the new password twice from stdin: it never appears in an argument list.
    let mut child = Command::new("smbpasswd")
        .arg("-c")
        .arg(&args.config)
        .args(["-s", "-a"])
        .arg(&u.user)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("smbpasswd: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let line = format!("{0}\n{0}\n", u.password);
        stdin
            .write_all(line.as_bytes())
            .map_err(|e| format!("smbpasswd for {}: {e}", u.user))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("smbpasswd: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "setting the Samba password of {} failed ({})",
            u.user, out.status
        ));
    }
    Ok(())
}

fn run(args: &Args, shares: &[Share], users: &[User], procs: &mut Vec<Proc>) -> Result<(), String> {
    for s in shares {
        let dir = args.root.join(&s.name);
        std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        if mounted(&dir) {
            return Err(format!("{} is already mounted", dir.display()));
        }
        let mut cmd = Command::new(&args.mount_bin);
        cmd.arg("--endpoint")
            .arg(args.endpoint.join(","))
            .arg("--fs")
            .arg(&s.fs)
            .arg("--allow-other");
        if let Some(t) = &args.token_file {
            cmd.arg("--token-file").arg(t);
        }
        if s.access == Access::Ro {
            cmd.arg("--read-only");
        }
        cmd.args(&args.mount_args).arg(&dir);
        procs.push(Proc::spawn(format!("mount of {}", s.name), &mut cmd)?);
    }
    let until = Instant::now() + Duration::from_secs(args.mount_timeout_secs);
    while !shares.iter().all(|s| mounted(&args.root.join(&s.name))) {
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
    tracing::info!(shares = shares.len(), "filesystems mounted");

    if let Some(dir) = args.config.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    for dir in ["/run/samba", "/var/lib/samba/private", "/var/cache/samba"] {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&args.config, smb::render(shares, &args.root, args.encrypt))
        .map_err(|e| format!("{}: {e}", args.config.display()))?;
    for u in users {
        ensure_account(args, u)?;
    }
    tracing::info!(users = users.len(), "accounts ready");
    procs.push(Proc::spawn(
        "smbd",
        Command::new(&args.smbd_bin)
            .args(["--foreground", "--no-process-group", "--debug-stdout"])
            .arg(format!("--configfile={}", args.config.display())),
    )?);
    tracing::info!("serving SMB");

    while !stopping() {
        if let Some(why) = procs.iter_mut().find_map(Proc::exited) {
            return Err(why);
        }
        sleep(Duration::from_millis(250));
    }
    tracing::info!("stopping");
    Ok(())
}

/// Stops smbd, then unmounts each share (which sends its buffered writes).
fn shutdown(args: &Args, shares: &[Share], procs: &mut [Proc]) {
    for p in procs
        .iter_mut()
        .filter(|p| !p.name.starts_with("mount of "))
    {
        p.stop(Duration::from_secs(20));
    }
    for s in shares {
        supervise::unmount(&args.root.join(&s.name));
    }
    for p in procs.iter_mut() {
        p.stop(Duration::from_secs(30));
    }
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "atlas_native_smb=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    supervise::handle_signals();
    let (shares, users) = match load(&args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("atlas-native-smb: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut procs = Vec::new();
    let result = run(&args, &shares, &users, &mut procs);
    shutdown(&args, &shares, &mut procs);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("atlas-native-smb: {e}");
            ExitCode::FAILURE
        }
    }
}
