// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! `atlasctl checkpoint seal` — seal a finished training checkpoint on an atlas-native filesystem.
//!
//! Thin glue over the existing REST API (`docs/CHECKPOINTS.md`): check that the checkpoint is
//! complete, take a filesystem snapshot (`POST /volumes/{id}/snapshots`), retention-lock it
//! (`POST /snapshots/{id}/lock`) and prune older snapshots this command made
//! (`DELETE /snapshots/{id}`). No server endpoint is added: the gateway has no file-stat or
//! file-read route for filesystems, so the completion check reads the filesystem through a local
//! mount (`--root`), or is skipped on the caller's word (`--assume-complete`).

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{json, Value};

/// Every snapshot this command creates is named `ckpt.<run>.<step>.<hash>`; pruning touches only
/// names that parse as exactly that.
pub const NAME_PREFIX: &str = "ckpt";
/// The longest lock one gateway call accepts is ten years.
pub const MAX_LOCK_DAYS: u32 = 3650;
/// Exit code: the completion marker is missing, nothing was created.
pub const EXIT_REFUSED: i32 = 3;
/// Exit code: the snapshot is sealed but pruning an old one failed for a reason other than a lock.
pub const EXIT_PRUNE_FAILED: i32 = 4;

// Names read from the PyTorch / Megatron-LM / Orbax sources; see docs/CHECKPOINTS.md.
const DCP_METADATA: &str = ".metadata";
const DCP_METADATA_TMP: &str = ".metadata.tmp";
const DCP_DATA_SUFFIX: &str = ".distcp";
const MEGATRON_TRACKER: &str = "latest_checkpointed_iteration.txt";
const ORBAX_TMP_SUFFIX: &str = ".orbax-checkpoint-tmp";
const ORBAX_COMMIT: &str = "commit_success.txt";

#[derive(Subcommand)]
pub enum CheckpointCommand {
    /// Snapshot a completed checkpoint, lock the snapshot, prune older sealed checkpoints
    Seal(SealArgs),
}

#[derive(Args)]
#[command(group(
    clap::ArgGroup::new("completion").required(true).multiple(false)
        .args(["marker", "preset", "assume_complete"])
))]
pub struct SealArgs {
    /// The atlas-native filesystem volume (`vol_native_fs_...`)
    #[arg(long)]
    pub volume: String,
    /// The checkpoint directory inside the filesystem, e.g. /runs/llama/iter_0001000
    #[arg(long)]
    pub path: String,
    /// Sealed only when this file exists inside the checkpoint directory
    #[arg(long)]
    pub marker: Option<String>,
    /// Completion check for a framework's on-disk layout
    #[arg(long, value_enum)]
    pub preset: Option<Preset>,
    /// Skip the completion check: the caller vouches the checkpoint is complete
    #[arg(long)]
    pub assume_complete: bool,
    /// Local directory where the filesystem is mounted (atlas-native-mount or NFS); the
    /// completion check reads `<root>/<path>` there
    #[arg(
        long,
        conflicts_with = "assume_complete",
        required_unless_present = "assume_complete"
    )]
    pub root: Option<PathBuf>,
    /// Retention lock in days (extend-only; 0 = do not lock)
    #[arg(long, default_value_t = 7)]
    pub lock_days: u32,
    /// Keep the newest N snapshots of this run made by this command; delete older unlocked ones
    #[arg(long)]
    pub keep_last: Option<usize>,
    /// Training run id grouping the snapshots for --keep-last (default: the checkpoint's parent
    /// directory)
    #[arg(long)]
    pub run_id: Option<String>,
    /// Check and print what would happen; change nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Preset {
    /// PyTorch `torch.distributed.checkpoint` FileSystemWriter: `.metadata` + `*.distcp`
    Dcp,
    /// Megatron-LM: `iter_NNNNNNN` plus the parent's `latest_checkpointed_iteration.txt`
    Megatron,
    /// Orbax: `commit_success.txt` in a directory that is not an `.orbax-checkpoint-tmp` one
    Orbax,
}

/// How the checkpoint is judged complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Completion {
    /// A file (relative to the checkpoint directory) that must exist.
    Marker(String),
    Preset(Preset),
    /// Not checked.
    Assumed,
}

/// A normalised absolute path inside the filesystem, never the root itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsPath(Vec<String>);

impl FsPath {
    pub fn parse(s: &str) -> Result<Self> {
        let mut parts = Vec::new();
        for c in s.split('/') {
            match c {
                "" | "." => {}
                ".." => bail!("--path {s:?} must not contain '..'"),
                c if c.contains('\0') => bail!("--path contains a NUL byte"),
                c => parts.push(c.to_string()),
            }
        }
        if parts.is_empty() {
            bail!("--path must name a checkpoint directory below the filesystem root");
        }
        Ok(Self(parts))
    }

    pub fn display(&self) -> String {
        format!("/{}", self.0.join("/"))
    }

    pub fn name(&self) -> &str {
        self.0.last().map(String::as_str).unwrap_or("")
    }

    fn parent_parts(&self) -> &[String] {
        &self.0[..self.0.len() - 1]
    }

    pub fn local(&self, root: &Path) -> PathBuf {
        self.0.iter().fold(root.to_path_buf(), |p, c| p.join(c))
    }

    fn local_parent(&self, root: &Path) -> PathBuf {
        self.parent_parts()
            .iter()
            .fold(root.to_path_buf(), |p, c| p.join(c))
    }
}

/// A `--marker` is a file below the checkpoint directory.
pub fn validate_marker(m: &str) -> Result<String> {
    if m.starts_with('/') {
        bail!("--marker {m:?} must be relative to the checkpoint directory");
    }
    let parts: Vec<&str> = m
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    if parts.is_empty() || parts.contains(&"..") || m.contains('\0') {
        bail!("--marker {m:?} must be a plain relative path without '..'");
    }
    Ok(parts.join("/"))
}

// ---- naming ----

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Last `n` bytes of an ASCII slug, so the name stays short without losing the tail that differs.
fn tail(s: &str, n: usize) -> String {
    let t = if s.len() > n { &s[s.len() - n..] } else { s };
    t.trim_matches('-').to_string()
}

/// FNV-1a over the normalised path, folded to 32 bits. It only tells apart two paths whose slugs
/// collide (`a_b` and `a-b`); it is not a security boundary.
fn path_hash(path: &str) -> u32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h ^ (h >> 32)) as u32
}

/// The run key snapshots are grouped by for `--keep-last`.
pub fn run_key(path: &FsPath, run_id: Option<&str>) -> Result<String> {
    match run_id {
        Some(r) => {
            let s = tail(&slug(r), 48);
            if s.is_empty() {
                bail!("--run-id {r:?} has no letters or digits");
            }
            Ok(s)
        }
        None => {
            let s = tail(&slug(&path.parent_parts().join("/")), 48);
            Ok(if s.is_empty() { "root".into() } else { s })
        }
    }
}

/// Deterministic snapshot name for a checkpoint: `ckpt.<run>.<step>.<hash of the path>`.
pub fn snapshot_name(path: &FsPath, run_id: Option<&str>) -> Result<String> {
    let run = run_key(path, run_id)?;
    let step = tail(&slug(path.name()), 48);
    let step = if step.is_empty() {
        "step".to_string()
    } else {
        step
    };
    Ok(format!(
        "{NAME_PREFIX}.{run}.{step}.{:08x}",
        path_hash(&path.display())
    ))
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedName {
    pub run: String,
    pub step_num: Option<u64>,
}

/// Parse a name made by [`snapshot_name`]; anything else is not ours and is never pruned.
pub fn parse_name(name: &str) -> Option<ParsedName> {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() != 4 || parts[0] != NAME_PREFIX {
        return None;
    }
    let plain = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let hash = parts[3];
    if !plain(parts[1])
        || !plain(parts[2])
        || hash.len() != 8
        || !hash.chars().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    let step = parts[2];
    let digits: String = step
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Some(ParsedName {
        run: parts[1].to_string(),
        step_num: digits.parse().ok(),
    })
}

// ---- completion ----

fn is_file(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

fn read_small(p: &Path) -> Result<String, String> {
    let mut buf = String::new();
    std::fs::File::open(p)
        .and_then(|f| f.take(256).read_to_string(&mut buf))
        .map_err(|e| format!("cannot read {}: {e}", p.display()))?;
    Ok(buf.trim().to_string())
}

/// `Ok(what was seen)` when the checkpoint looks complete from `root`, `Err(why not)` otherwise.
pub fn check_completion(c: &Completion, root: &Path, dir: &FsPath) -> Result<String, String> {
    if *c == Completion::Assumed {
        return Ok("not checked (--assume-complete)".into());
    }
    let local = dir.local(root);
    if !local.is_dir() {
        return Err(format!(
            "{} is not a directory (is --root the mount point of this filesystem?)",
            local.display()
        ));
    }
    match c {
        Completion::Assumed => unreachable!(),
        Completion::Marker(m) => {
            if is_file(&local.join(m)) {
                Ok(format!("marker {m} present"))
            } else {
                Err(format!(
                    "completion marker {m} is missing in {}",
                    local.display()
                ))
            }
        }
        Completion::Preset(Preset::Dcp) => {
            if local.join(DCP_METADATA_TMP).exists() {
                return Err(format!(
                    "{DCP_METADATA_TMP} exists in {}: the writer has not finished (or is overwriting)",
                    local.display()
                ));
            }
            if !is_file(&local.join(DCP_METADATA)) {
                return Err(format!("{DCP_METADATA} is missing in {}", local.display()));
            }
            let has_data = std::fs::read_dir(&local)
                .map(|rd| {
                    rd.flatten()
                        .any(|e| e.file_name().to_string_lossy().ends_with(DCP_DATA_SUFFIX))
                })
                .unwrap_or(false);
            if !has_data {
                return Err(format!(
                    "{DCP_METADATA} exists but there is no *{DCP_DATA_SUFFIX} file in {}",
                    local.display()
                ));
            }
            Ok(format!("{DCP_METADATA} and *{DCP_DATA_SUFFIX} present"))
        }
        Completion::Preset(Preset::Megatron) => {
            let name = dir.name();
            let want = if name == "release" {
                None
            } else {
                let n = name
                    .strip_prefix("iter_")
                    .filter(|d| d.len() >= 7 && d.chars().all(|c| c.is_ascii_digit()))
                    .and_then(|d| d.parse::<u64>().ok())
                    .ok_or_else(|| format!("{name:?} is not a Megatron checkpoint directory (iter_NNNNNNN or release)"))?;
                Some(n)
            };
            let tracker = dir.local_parent(root).join(MEGATRON_TRACKER);
            if !is_file(&tracker) {
                return Err(format!("{} is missing", tracker.display()));
            }
            let seen = read_small(&tracker)?;
            let ok = match want {
                None => seen == "release",
                Some(n) => seen.parse::<u64>().ok() == Some(n),
            };
            if ok {
                Ok(format!("{MEGATRON_TRACKER} says {seen:?}"))
            } else {
                Err(format!(
                    "{MEGATRON_TRACKER} says {seen:?}, which is not this checkpoint ({name}); it holds only the latest save"
                ))
            }
        }
        Completion::Preset(Preset::Orbax) => {
            if dir.name().contains(ORBAX_TMP_SUFFIX) {
                return Err(format!(
                    "{} is an uncommitted Orbax temporary directory",
                    dir.name()
                ));
            }
            if is_file(&local.join(ORBAX_COMMIT)) {
                Ok(format!("{ORBAX_COMMIT} present"))
            } else {
                Err(format!("{ORBAX_COMMIT} is missing in {}", local.display()))
            }
        }
    }
}

// ---- pruning ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapRow {
    pub id: String,
    pub name: String,
    pub created_at: String,
}

/// Snapshots of `run` made by this command beyond the newest `keep`, oldest last. The snapshot
/// sealed by this invocation is never a candidate. Order is creation time (second resolution),
/// then the step number in the name, then the name.
pub fn prune_candidates(rows: &[SnapRow], run: &str, keep: usize, sealed_id: &str) -> Vec<SnapRow> {
    let mut ours: Vec<(&SnapRow, Option<u64>)> = rows
        .iter()
        .filter_map(|r| {
            parse_name(&r.name)
                .filter(|p| p.run == run)
                .map(|p| (r, p.step_num))
        })
        .collect();
    ours.sort_by(|a, b| (&b.0.created_at, b.1, &b.0.name).cmp(&(&a.0.created_at, a.1, &a.0.name)));
    ours.into_iter()
        .skip(keep)
        .filter(|(r, _)| r.id != sealed_id)
        .map(|(r, _)| r.clone())
        .collect()
}

// ---- REST ----

struct Api<'a> {
    client: &'a reqwest::Client,
    base: &'a str,
    token: Option<&'a str>,
}

impl Api<'_> {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let url = format!("{}/api/atlas/v1{path}", self.base);
        let mut req = self.client.request(method, &url);
        if let Some(b) = body {
            req = req.json(&b);
        }
        if let Some(t) = self.token {
            req = req.bearer_auth(t);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("request to {url}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let v = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "error": text }));
        Ok((status, v))
    }
}

fn err_text(v: &Value) -> String {
    // The gateway's envelope is {"error": {"code", "message"}}; a non-JSON body is kept as text.
    let e = v.get("error");
    e.and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .or_else(|| e.and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}

fn snap_rows(v: &Value, volume: &str) -> Vec<SnapRow> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter(|s| s["volume_id"] == volume)
                .map(|s| SnapRow {
                    id: s["id"].as_str().unwrap_or_default().to_string(),
                    name: s["name"].as_str().unwrap_or_default().to_string(),
                    created_at: s["created_at"].as_str().unwrap_or_default().to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Run `checkpoint seal`; returns the process exit code.
pub async fn run(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    cmd: CheckpointCommand,
) -> Result<i32> {
    let CheckpointCommand::Seal(a) = cmd;
    let path = FsPath::parse(&a.path)?;
    let completion = match (&a.marker, a.preset, a.assume_complete) {
        (Some(m), _, _) => Completion::Marker(validate_marker(m)?),
        (_, Some(p), _) => Completion::Preset(p),
        _ => Completion::Assumed,
    };
    if a.lock_days > MAX_LOCK_DAYS {
        bail!("--lock-days must be at most {MAX_LOCK_DAYS}");
    }
    if a.keep_last == Some(0) {
        bail!("--keep-last must be at least 1");
    }
    let name = snapshot_name(&path, a.run_id.as_deref())?;
    let run = run_key(&path, a.run_id.as_deref())?;

    // 1. The checkpoint must be complete as far as the mount shows.
    let seen = match (&a.root, &completion) {
        (_, Completion::Assumed) => "not checked (--assume-complete)".to_string(),
        (Some(root), c) => match check_completion(c, root, &path) {
            Ok(s) => s,
            Err(why) => {
                eprintln!("refusing to seal {}: {why}", path.display());
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "sealed": false, "refused": true, "reason": why, "path": path.display(),
                    }))?
                );
                return Ok(EXIT_REFUSED);
            }
        },
        (None, _) => bail!("--root is required unless --assume-complete is given"),
    };

    // 2. The volume must be a filesystem; look for an earlier seal of this checkpoint.
    let api = Api {
        client,
        base: base.trim_end_matches('/'),
        token,
    };
    let (st, vol) = api
        .call(
            reqwest::Method::GET,
            &format!("/volumes/{}", a.volume),
            None,
        )
        .await?;
    if st == 404 {
        bail!("volume {} not found", a.volume);
    }
    if st != 200 {
        bail!("GET /volumes/{}: HTTP {st}: {}", a.volume, err_text(&vol));
    }
    if vol["kind"] != "filesystem" {
        bail!(
            "volume {} is a {} volume; only filesystem volumes can be sealed",
            a.volume,
            vol["kind"].as_str().unwrap_or("unknown")
        );
    }
    let (st, snaps) = api.call(reqwest::Method::GET, "/snapshots", None).await?;
    if st != 200 {
        bail!("GET /snapshots: HTTP {st}: {}", err_text(&snaps));
    }
    let mut rows = snap_rows(&snaps, &a.volume);
    // The list is newest first; if a race made two, the oldest is the one that was sealed first.
    let existing = rows.iter().rev().find(|r| r.name == name).cloned();

    let lock_secs = u64::from(a.lock_days) * 86_400;
    let sealed_id = existing
        .as_ref()
        .map(|r| r.id.clone())
        .unwrap_or_else(|| "(new)".into());
    if existing.is_none() {
        // Stands in for the snapshot about to be made so that --keep-last counts it as the newest.
        rows.push(SnapRow {
            id: sealed_id.clone(),
            name: name.clone(),
            created_at: "9999-12-31 23:59:59".into(),
        });
    }
    let candidates = a
        .keep_last
        .map(|k| prune_candidates(&rows, &run, k, &sealed_id))
        .unwrap_or_default();

    if a.dry_run {
        eprintln!("dry run: nothing changed");
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "dry_run": true,
                "volume": a.volume,
                "path": path.display(),
                "completion": seen,
                "snapshot_name": name,
                "run": run,
                "snapshot": if existing.is_some() { "exists, would be reused" } else { "would be created" },
                "snapshot_id": existing.as_ref().map(|r| r.id.clone()),
                "lock_days": a.lock_days,
                "would_try_to_delete": candidates.iter().map(|r| json!({"id": r.id, "name": r.name})).collect::<Vec<_>>(),
                "note": "a locked snapshot, or one with clones, is refused by the gateway and kept",
            }))?
        );
        return Ok(0);
    }

    // 3. Snapshot (reused when this checkpoint was sealed before).
    let (snapshot_id, created) = match existing {
        Some(r) => (r.id, false),
        None => {
            let (st, body) = api
                .call(
                    reqwest::Method::POST,
                    &format!("/volumes/{}/snapshots", a.volume),
                    Some(json!({ "name": name })),
                )
                .await?;
            if st != 201 {
                bail!("create snapshot: HTTP {st}: {}", err_text(&body));
            }
            let id = body["snapshot_id"]
                .as_str()
                .context("snapshot response has no snapshot_id")?
                .to_string();
            (id, true)
        }
    };
    eprintln!(
        "snapshot {snapshot_id} ({name}) {}",
        if created {
            "created"
        } else {
            "already existed"
        }
    );

    // 4. Lock. A lock can only be extended, so a re-run with a shorter --lock-days is not an error.
    let mut lock = json!({ "days": a.lock_days, "locked": false });
    if lock_secs > 0 {
        let (st, body) = api
            .call(
                reqwest::Method::POST,
                &format!("/snapshots/{snapshot_id}/lock"),
                Some(json!({ "retain_secs": lock_secs })),
            )
            .await?;
        match st {
            200 => lock = json!({ "days": a.lock_days, "locked": true, "retain_until_unix": body["retain_until_unix"] }),
            409 if err_text(&body).contains("cannot be shortened") => {
                lock = json!({ "days": a.lock_days, "locked": true, "note": "already locked for longer" });
            }
            _ => bail!(
                "snapshot {snapshot_id} exists but is NOT locked: lock: HTTP {st}: {} (re-run to retry)",
                err_text(&body)
            ),
        }
    }

    // 5. Prune.
    let (mut pruned, mut retained, mut failed) = (Vec::new(), Vec::new(), Vec::new());
    for r in candidates {
        let (st, body) = api
            .call(
                reqwest::Method::DELETE,
                &format!("/snapshots/{}", r.id),
                None,
            )
            .await?;
        let item = json!({ "id": r.id, "name": r.name });
        match st {
            200 | 404 => pruned.push(item),
            409 => retained.push(json!({ "id": r.id, "name": r.name, "reason": err_text(&body) })),
            _ => failed.push(
                json!({ "id": r.id, "name": r.name, "status": st, "error": err_text(&body) }),
            ),
        }
    }

    let code = if failed.is_empty() {
        0
    } else {
        EXIT_PRUNE_FAILED
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "sealed": true,
            "volume": a.volume,
            "path": path.display(),
            "completion": seen,
            "snapshot_id": snapshot_id,
            "snapshot_name": name,
            "created": created,
            "lock": lock,
            "run": run,
            "pruned": pruned,
            "retained": retained,
            "prune_failed": failed,
        }))?
    );
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atlasctl-ckpt-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(p: &Path, content: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    #[test]
    fn fs_path_is_normalised_and_never_escapes() {
        assert_eq!(
            FsPath::parse("/runs//llama/./iter_0000100/")
                .unwrap()
                .display(),
            "/runs/llama/iter_0000100"
        );
        assert_eq!(FsPath::parse("ckpt/1").unwrap().display(), "/ckpt/1");
        assert!(FsPath::parse("/a/../b").is_err());
        assert!(FsPath::parse("/").is_err());
        assert!(FsPath::parse("").is_err());
    }

    #[test]
    fn marker_must_be_a_plain_relative_file() {
        assert_eq!(validate_marker("./DONE").unwrap(), "DONE");
        assert_eq!(validate_marker("meta//ok.json").unwrap(), "meta/ok.json");
        assert!(validate_marker("/etc/passwd").is_err());
        assert!(validate_marker("../latest.txt").is_err());
        assert!(validate_marker("").is_err());
    }

    #[test]
    fn name_is_deterministic_and_carries_run_and_step() {
        let p = FsPath::parse("/runs/llama-70b/iter_0001000").unwrap();
        let a = snapshot_name(&p, None).unwrap();
        assert_eq!(a, snapshot_name(&p, None).unwrap());
        assert!(a.starts_with("ckpt.runs-llama-70b.iter-0001000."), "{a}");
        let parsed = parse_name(&a).unwrap();
        assert_eq!(parsed.run, "runs-llama-70b");
        assert_eq!(parsed.step_num, Some(1000));
        // An explicit run id replaces the parent-directory default.
        let b = snapshot_name(&p, Some("llama 70b/v2")).unwrap();
        assert!(b.starts_with("ckpt.llama-70b-v2.iter-0001000."), "{b}");
        assert!(snapshot_name(&p, Some("///")).is_err());
        // A top-level checkpoint still gets a run key.
        assert!(snapshot_name(&FsPath::parse("/step5").unwrap(), None)
            .unwrap()
            .starts_with("ckpt.root.step5."));
    }

    #[test]
    fn paths_whose_slugs_collide_get_different_names() {
        let a = snapshot_name(&FsPath::parse("/r/step_1").unwrap(), None).unwrap();
        let b = snapshot_name(&FsPath::parse("/r/step-1").unwrap(), None).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.rsplit_once('.').unwrap().0, b.rsplit_once('.').unwrap().0);
    }

    #[test]
    fn only_names_made_by_this_command_parse() {
        assert!(parse_name("ckpt.run.step1.0123abcd").is_some());
        for bad in [
            "manual",
            "ckpt.run.step1",
            "ckpt.run.step1.xyz",
            "ckpt..s.0123abcd",
            "snap.run.s.0123abcd",
            "ckpt.a.b.c.0123abcd",
            "ckpt.run.step1.0123abcdef",
        ] {
            assert!(parse_name(bad).is_none(), "{bad}");
        }
        assert_eq!(
            parse_name("ckpt.run.final.0123abcd").unwrap().step_num,
            None
        );
    }

    #[test]
    fn dcp_needs_metadata_and_data_and_no_tmp() {
        let root = tmp("dcp");
        let dir = FsPath::parse("/run/step_100").unwrap();
        let local = dir.local(&root);
        let c = Completion::Preset(Preset::Dcp);
        touch(&local.join("__0_0.distcp"), "x");
        assert!(check_completion(&c, &root, &dir)
            .unwrap_err()
            .contains(".metadata is missing"));
        touch(&local.join(".metadata.tmp"), "x");
        assert!(check_completion(&c, &root, &dir)
            .unwrap_err()
            .contains("has not finished"));
        fs::rename(local.join(".metadata.tmp"), local.join(".metadata")).unwrap();
        assert!(check_completion(&c, &root, &dir).is_ok());
        // A rewrite in progress leaves the old .metadata beside a new .metadata.tmp.
        touch(&local.join(".metadata.tmp"), "x");
        assert!(check_completion(&c, &root, &dir).is_err());
        fs::remove_file(local.join(".metadata.tmp")).unwrap();
        fs::remove_file(local.join("__0_0.distcp")).unwrap();
        assert!(check_completion(&c, &root, &dir)
            .unwrap_err()
            .contains("no *.distcp"));
        assert!(
            check_completion(&c, &root, &FsPath::parse("/run/none").unwrap())
                .unwrap_err()
                .contains("not a directory")
        );
    }

    #[test]
    fn megatron_tracker_must_name_this_iteration() {
        let root = tmp("megatron");
        let dir = FsPath::parse("/run/iter_0000200").unwrap();
        fs::create_dir_all(dir.local(&root)).unwrap();
        let c = Completion::Preset(Preset::Megatron);
        assert!(check_completion(&c, &root, &dir)
            .unwrap_err()
            .contains("latest_checkpointed_iteration.txt"));
        let tracker = root.join("run").join(MEGATRON_TRACKER);
        touch(&tracker, "100");
        assert!(check_completion(&c, &root, &dir)
            .unwrap_err()
            .contains("holds only the latest"));
        touch(&tracker, "200\n");
        assert!(check_completion(&c, &root, &dir).is_ok());
        touch(&tracker, "release");
        assert!(check_completion(&c, &root, &dir).is_err());
        // The release checkpoint.
        let rel = FsPath::parse("/run/release").unwrap();
        fs::create_dir_all(rel.local(&root)).unwrap();
        assert!(check_completion(&c, &root, &rel).is_ok());
        touch(&tracker, "200");
        assert!(check_completion(&c, &root, &rel).is_err());
        // Not a Megatron directory name.
        let other = FsPath::parse("/run/step_5").unwrap();
        fs::create_dir_all(other.local(&root)).unwrap();
        assert!(check_completion(&c, &root, &other)
            .unwrap_err()
            .contains("not a Megatron"));
    }

    #[test]
    fn orbax_refuses_temporary_directories() {
        let root = tmp("orbax");
        let c = Completion::Preset(Preset::Orbax);
        let tmpdir = FsPath::parse("/run/100.orbax-checkpoint-tmp").unwrap();
        touch(&tmpdir.local(&root).join(ORBAX_COMMIT), "ok");
        assert!(check_completion(&c, &root, &tmpdir)
            .unwrap_err()
            .contains("temporary"));
        let dir = FsPath::parse("/run/100").unwrap();
        fs::create_dir_all(dir.local(&root)).unwrap();
        assert!(check_completion(&c, &root, &dir)
            .unwrap_err()
            .contains("commit_success.txt is missing"));
        touch(&dir.local(&root).join(ORBAX_COMMIT), "ok");
        assert!(check_completion(&c, &root, &dir).is_ok());
    }

    #[test]
    fn explicit_marker_and_assumed_completion() {
        let root = tmp("marker");
        let dir = FsPath::parse("/run/s1").unwrap();
        fs::create_dir_all(dir.local(&root).join("DONE")).unwrap();
        let c = Completion::Marker("DONE".into());
        assert!(
            check_completion(&c, &root, &dir).is_err(),
            "a directory is not a marker file"
        );
        fs::remove_dir(dir.local(&root).join("DONE")).unwrap();
        touch(&dir.local(&root).join("DONE"), "");
        assert!(check_completion(&c, &root, &dir).is_ok());
        assert!(check_completion(
            &Completion::Assumed,
            &root,
            &FsPath::parse("/nowhere").unwrap()
        )
        .is_ok());
    }

    fn row(id: &str, run: &str, step: u32, created: &str) -> SnapRow {
        SnapRow {
            id: id.into(),
            name: format!("ckpt.{run}.step{step}.0123abcd"),
            created_at: created.into(),
        }
    }

    #[test]
    fn prune_keeps_newest_and_touches_only_our_run() {
        let rows = vec![
            row("a", "r", 1, "2026-01-01 00:00:01"),
            row("b", "r", 2, "2026-01-01 00:00:02"),
            row("c", "r", 3, "2026-01-01 00:00:03"),
            row("o", "other", 0, "2025-01-01 00:00:00"),
            SnapRow {
                id: "m".into(),
                name: "manual".into(),
                created_at: "2020-01-01 00:00:00".into(),
            },
        ];
        let ids = |v: Vec<SnapRow>| v.into_iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(ids(prune_candidates(&rows, "r", 2, "c")), ["a"]);
        assert_eq!(ids(prune_candidates(&rows, "r", 1, "c")), ["b", "a"]);
        assert!(prune_candidates(&rows, "r", 3, "c").is_empty());
        // The snapshot sealed now is never a candidate, even when it ranks old (a backfill).
        assert_eq!(ids(prune_candidates(&rows, "r", 1, "a")), ["b"]);
    }

    #[test]
    fn prune_breaks_second_level_ties_by_step_number() {
        let same = "2026-01-01 00:00:00";
        let rows = vec![
            row("s2", "r", 2, same),
            row("s10", "r", 10, same),
            row("s1", "r", 1, same),
        ];
        let out: Vec<String> = prune_candidates(&rows, "r", 1, "none")
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(out, ["s2", "s1"]);
    }
}
