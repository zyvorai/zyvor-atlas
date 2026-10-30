// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Safe `ceph`/`rbd` command wrappers.
//!
//! Security rule (PDF §17.3): pass arguments as an **array only**; never build a shell string.
//! Modeled on `machina/agent/src/provision_ops.rs`, but async and JSON-parsing.

use std::time::Duration;

use atlas_driver_core::DriverError;

/// `radosgw-admin` talks to RGW over its admin ops socket/REST API rather than directly to the
/// mons, so it can hang far longer than an `rbd`/`ceph` CLI call when RGW is slow or unresponsive
/// — and unlike those, nothing here waits on a multi-stage operation the caller explicitly wants
/// to block on. Bounding every `radosgw-admin` invocation at this shared layer means no caller
/// (present or future) can wedge the single-threaded job engine's worker indefinitely — a hung
/// `bucket.create` quota-set call was observed blocking every other tenant's jobs for the full
/// 2h `ATLAS_JOB_TIMEOUT_SECS` ceiling until manually cancelled via `POST /jobs/{id}/cancel`.
const RADOSGW_ADMIN_TIMEOUT: Duration = Duration::from_secs(12);

/// Build a `Command` for a ceph/rbd/radosgw-admin binary with `kill_on_drop(true)` — if the
/// enclosing future is dropped (job cancellation, `tokio::select!` racing a timeout), the child
/// process is killed rather than left running as an orphan holding cluster-side locks/watchers.
fn cmd(bin: &str) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(bin);
    c.kill_on_drop(true);
    c
}

/// Run `ceph <args...> --format json` and parse stdout as JSON.
pub async fn ceph_cmd(args: &[&str]) -> Result<serde_json::Value, DriverError> {
    run_json("ceph", args).await
}

/// `ceph osd ok-to-stop <ids...>`: whether stopping all of `osd_ids` at once keeps every PG
/// available. Ceph answers "no" with a non-zero exit status and JSON on stdout, so a refusal is a
/// normal result here (`ok_to_stop: false`), not an error.
pub async fn ceph_osd_ok_to_stop(osd_ids: &[i64]) -> Result<serde_json::Value, DriverError> {
    let ids: Vec<String> = osd_ids.iter().map(|i| i.to_string()).collect();
    let mut args = vec!["osd", "ok-to-stop"];
    args.extend(ids.iter().map(String::as_str));
    let output = cmd("ceph")
        .args(&args)
        .arg("--format")
        .arg("json")
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `ceph`: {e}")))?;
    interpret_ok_to_stop(
        output.status.success(),
        &output.stdout,
        &String::from_utf8_lossy(&output.stderr),
    )
}

/// Turn the outcome of `ceph osd ok-to-stop` into a result with an explicit `ok_to_stop` flag.
/// A failure that came with JSON is Ceph saying "not safe"; a failure without it (unknown OSD,
/// no quorum, no `ceph` access) is a real error and keeps Ceph's own message.
fn interpret_ok_to_stop(
    success: bool,
    stdout: &[u8],
    stderr: &str,
) -> Result<serde_json::Value, DriverError> {
    let parsed: Option<serde_json::Value> = serde_json::from_slice(stdout).ok();
    match (success, parsed) {
        (true, Some(mut v)) => {
            if let Some(o) = v.as_object_mut() {
                o.entry("ok_to_stop").or_insert(serde_json::Value::Bool(true));
            }
            Ok(v)
        }
        (false, Some(mut v)) if v.is_object() => {
            v["ok_to_stop"] = serde_json::Value::Bool(false);
            if !stderr.trim().is_empty() {
                v["reason"] = serde_json::Value::String(stderr.trim().to_string());
            }
            Ok(v)
        }
        (true, None) => Ok(serde_json::json!({ "ok_to_stop": true })),
        _ => Err(DriverError::Backend(format!(
            "`ceph osd ok-to-stop` failed: {}",
            stderr.trim()
        ))),
    }
}

/// Run `rbd <args...> --format json` and parse stdout as JSON.
pub async fn rbd_cmd(args: &[&str]) -> Result<serde_json::Value, DriverError> {
    run_json("rbd", args).await
}

/// Create an RBD snapshot `pool/image@snap` (idempotent-ish; errors if it already exists).
pub async fn rbd_snap_create(pool: &str, image: &str, snap: &str) -> Result<(), DriverError> {
    let spec = format!("{pool}/{image}@{snap}");
    let output = cmd("rbd")
        .args(["snap", "create", &spec])
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!(
            "rbd snap create {spec}: {stderr}"
        )));
    }
    Ok(())
}

/// Day-2 cross-cluster DR: per-image RBD mirroring op — `rbd mirror image enable <pool>/<image>
/// <mode>` / `disable` / `promote` [`--force`] / `demote` / `resync`, or `join-pool` /
/// `leave-pool` (enable / disable journaling, which is what mirrors an image in a pool-mode pool). Real ops need a live second Ceph cluster
/// (see `docs/DR.md`). `force` only applies to `promote` (split-brain / non-clean failover).
pub async fn rbd_mirror_op(
    op: &str,
    pool: &str,
    image: &str,
    mode: &str,
    force: bool,
) -> Result<(), DriverError> {
    match op {
        "join-pool" => return rbd_enable_journaling(pool, image).await,
        "leave-pool" => {
            return run_rbd(&[
                "feature",
                "disable",
                &format!("{pool}/{image}"),
                "journaling",
            ])
            .await
        }
        _ => {}
    }
    let spec = format!("{pool}/{image}");
    let args: Vec<String> = match op {
        "enable" => vec![
            "mirror".into(),
            "image".into(),
            "enable".into(),
            spec.clone(),
            mode.into(),
        ],
        "disable" => vec![
            "mirror".into(),
            "image".into(),
            "disable".into(),
            spec.clone(),
        ],
        "promote" if force => vec![
            "mirror".into(),
            "image".into(),
            "promote".into(),
            spec.clone(),
            "--force".into(),
        ],
        "promote" => vec![
            "mirror".into(),
            "image".into(),
            "promote".into(),
            spec.clone(),
        ],
        "demote" => vec![
            "mirror".into(),
            "image".into(),
            "demote".into(),
            spec.clone(),
        ],
        "resync" => vec![
            "mirror".into(),
            "image".into(),
            "resync".into(),
            spec.clone(),
        ],
        other => return Err(DriverError::Backend(format!("unknown mirror op: {other}"))),
    };
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = cmd("rbd")
        .args(&refs)
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!(
            "rbd {}: {stderr}",
            refs.join(" ")
        )));
    }
    Ok(())
}

/// Day-2 per-image QoS: `rbd config image set <pool>/<image> rbd_qos_{iops,bps}_limit <n>` to cap a
/// noisy volume's IOPS / bandwidth. A limit of `0` removes that cap. Only the provided limits are set.
pub async fn rbd_qos_set(
    pool: &str,
    image: &str,
    iops_limit: Option<i64>,
    bps_limit: Option<i64>,
) -> Result<(), DriverError> {
    let spec = format!("{pool}/{image}");
    for (key, val) in [
        ("rbd_qos_iops_limit", iops_limit),
        ("rbd_qos_bps_limit", bps_limit),
    ] {
        let Some(v) = val else { continue };
        let vs = v.to_string();
        let output = cmd("rbd")
            .args(["config", "image", "set", &spec, key, &vs])
            .output()
            .await
            .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(DriverError::Backend(format!(
                "rbd config image set {spec} {key} {vs}: {stderr}"
            )));
        }
    }
    Ok(())
}

/// Day-2 OSD maintenance op: `ceph osd out|in <id>` (drain / return an OSD) or
/// `ceph osd reweight <id> <weight>` (rebalance data off/onto an OSD; weight in [0,1]).
pub async fn ceph_osd_op(op: &str, osd_id: i64, weight: Option<f64>) -> Result<(), DriverError> {
    let id = osd_id.to_string();
    let args: Vec<String> = match op {
        "out" => vec!["osd".into(), "out".into(), id],
        "in" => vec!["osd".into(), "in".into(), id],
        "reweight" => {
            let w =
                weight.ok_or_else(|| DriverError::Backend("reweight requires a weight".into()))?;
            if !(0.0..=1.0).contains(&w) {
                return Err(DriverError::Backend(
                    "reweight weight must be in [0.0, 1.0]".into(),
                ));
            }
            vec!["osd".into(), "reweight".into(), id, format!("{w}")]
        }
        other => return Err(DriverError::Backend(format!("unknown osd op: {other}"))),
    };
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = cmd("ceph")
        .args(&refs)
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `ceph`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!(
            "ceph {}: {stderr}",
            refs.join(" ")
        )));
    }
    Ok(())
}

/// Set + enable an RGW per-bucket quota via `radosgw-admin` (Rook doesn't always apply the OBC
/// additionalConfig quota, so Atlas enforces it directly with the admin keyring in the pod).
pub async fn radosgw_bucket_quota(
    bucket: &str,
    max_objects: Option<i64>,
    max_size: Option<&str>,
) -> Result<(), DriverError> {
    let mo = max_objects.map(|n| n.to_string());
    let mut set_args: Vec<&str> = vec!["quota", "set", "--bucket", bucket, "--quota-scope=bucket"];
    if let Some(ref n) = mo {
        set_args.push("--max-objects");
        set_args.push(n);
    }
    if let Some(sz) = max_size {
        set_args.push("--max-size");
        set_args.push(sz);
    }
    radosgw_admin(&set_args).await?;
    radosgw_admin(&[
        "quota",
        "enable",
        "--bucket",
        bucket,
        "--quota-scope=bucket",
    ])
    .await?;
    Ok(())
}

/// Run `radosgw-admin <args...> --format json` and parse stdout (e.g. `bucket stats`).
pub async fn radosgw_admin_json(args: &[&str]) -> Result<serde_json::Value, DriverError> {
    let output = tokio::time::timeout(
        RADOSGW_ADMIN_TIMEOUT,
        cmd("radosgw-admin")
            .args(args)
            .arg("--format")
            .arg("json")
            .output(),
    )
    .await
    .map_err(|_| DriverError::Unreachable("radosgw-admin timed out".into()))?
    .map_err(|e| DriverError::Unreachable(format!("failed to spawn `radosgw-admin`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!(
            "radosgw-admin {}: {stderr}",
            args.join(" ")
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| DriverError::Parse(format!("radosgw-admin json: {e}")))
}

/// List an RBD image's snapshot names (`rbd snap ls pool/image --format json`).
pub async fn rbd_snap_list(pool: &str, image: &str) -> Result<Vec<String>, DriverError> {
    let spec = format!("{pool}/{image}");
    let v = rbd_cmd(&["snap", "ls", &spec]).await?;
    Ok(v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default())
}

/// Roll an RBD image back to a snapshot (`rbd snap rollback pool/image@snap`). Destructive.
pub async fn rbd_snap_rollback(pool: &str, image: &str, snap: &str) -> Result<(), DriverError> {
    let spec = format!("{pool}/{image}@{snap}");
    let output = cmd("rbd")
        .args(["snap", "rollback", &spec])
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!(
            "rbd snap rollback {spec}: {stderr}"
        )));
    }
    Ok(())
}

async fn radosgw_admin(args: &[&str]) -> Result<(), DriverError> {
    let output = tokio::time::timeout(
        RADOSGW_ADMIN_TIMEOUT,
        cmd("radosgw-admin").args(args).output(),
    )
    .await
    .map_err(|_| DriverError::Unreachable("radosgw-admin timed out".into()))?
    .map_err(|e| DriverError::Unreachable(format!("failed to spawn `radosgw-admin`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!(
            "radosgw-admin {}: {stderr}",
            args.join(" ")
        )));
    }
    Ok(())
}

/// Create an RBD image directly (`rbd create pool/image --size <MiB>`) for non-CSI consumers.
pub async fn rbd_create(pool: &str, image: &str, size_bytes: i64) -> Result<(), DriverError> {
    let spec = format!("{pool}/{image}");
    // Round up so the image is never smaller than requested when size_bytes isn't MiB-aligned.
    // NOTE: `i64::div_ceil` is unstable (int_roundings); do not "simplify" to it.
    // `saturating_add` guards a caller-supplied size near `i64::MAX`: a plain `+` would overflow
    // (panics in debug, silently wraps to garbage in release — e.g. creating a 1 MiB image when
    // an enormous size was requested instead of erroring).
    #[allow(clippy::manual_div_ceil)]
    let mib_val = size_bytes.saturating_add(1024 * 1024 - 1) / (1024 * 1024);
    let mib = std::cmp::max(1, mib_val).to_string();
    let output = cmd("rbd")
        .args(["create", &spec, "--size", &mib])
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!("rbd create {spec}: {stderr}")));
    }
    Ok(())
}

/// Remove an RBD image directly (`rbd rm pool/image`; idempotent on "not found").
pub async fn rbd_remove(pool: &str, image: &str) -> Result<(), DriverError> {
    let spec = format!("{pool}/{image}");
    let output = cmd("rbd")
        .args(["rm", &spec])
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such file") || stderr.contains("does not exist") {
            return Ok(());
        }
        return Err(DriverError::Backend(format!(
            "rbd rm {spec}: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

/// Resize an RBD image (`rbd resize pool/image --size <MiB>`). Shrinking is refused by `rbd` unless
/// `allow_shrink` is set (day-2, guarded — a shrink can lose data past the new size).
pub async fn rbd_resize(
    pool: &str,
    image: &str,
    size_bytes: i64,
    allow_shrink: bool,
) -> Result<(), DriverError> {
    let spec = format!("{pool}/{image}");
    // Round up so a grow never under-shoots and a shrink never removes more than requested.
    // NOTE: `i64::div_ceil` is unstable (int_roundings); do not "simplify" to it.
    // `saturating_add` guards a caller-supplied size near `i64::MAX` (see `rbd_create` above).
    #[allow(clippy::manual_div_ceil)]
    let mib_val = size_bytes.saturating_add(1024 * 1024 - 1) / (1024 * 1024);
    let mib = std::cmp::max(1, mib_val).to_string();
    let mut args = vec!["resize", &spec, "--size", &mib];
    if allow_shrink {
        args.push("--allow-shrink");
    }
    let output = cmd("rbd")
        .args(&args)
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!("rbd resize {spec}: {stderr}")));
    }
    Ok(())
}

/// Day-2: migrate an RBD image to another pool (`rbd migration prepare`→`execute`→`commit`). Live,
/// online migration. UNVERIFIED against real Ceph (needs the source + dest pools present).
pub async fn rbd_migrate(pool: &str, image: &str, dest_pool: &str) -> Result<(), DriverError> {
    let src = format!("{pool}/{image}");
    let dst = format!("{dest_pool}/{image}");
    for stage in [
        vec!["migration", "prepare", &src, &dst],
        vec!["migration", "execute", &dst],
        vec!["migration", "commit", &dst],
    ] {
        let output = cmd("rbd")
            .args(&stage)
            .output()
            .await
            .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(DriverError::Backend(format!(
                "rbd {}: {stderr}",
                stage.join(" ")
            )));
        }
    }
    Ok(())
}

/// Flatten a cloned image so it no longer depends on its parent snapshot (`rbd flatten`).
pub async fn rbd_flatten(pool: &str, image: &str) -> Result<(), DriverError> {
    let spec = format!("{pool}/{image}");
    let output = cmd("rbd")
        .args(["flatten", &spec])
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!(
            "rbd flatten {spec}: {stderr}"
        )));
    }
    Ok(())
}

/// Total provisioned size of an RBD image in bytes (`rbd info pool/image --format json`).
pub async fn rbd_info_size(pool: &str, image: &str) -> Result<i64, DriverError> {
    let spec = format!("{pool}/{image}");
    let v = rbd_cmd(&["info", &spec]).await?;
    v.get("size")
        .and_then(|s| s.as_i64())
        .ok_or_else(|| DriverError::Parse(format!("rbd info {spec}: no size")))
}

/// Mirroring role of an RBD image from `rbd info pool/image --format json`: `Some(true)` primary,
/// `Some(false)` non-primary, `None` when mirroring is not enabled on the image.
pub async fn rbd_mirror_primary(pool: &str, image: &str) -> Result<Option<bool>, DriverError> {
    let spec = format!("{pool}/{image}");
    let v = rbd_cmd(&["info", &spec]).await?;
    Ok(v.get("mirroring")
        .filter(|m| m.get("state").and_then(|s| s.as_str()) == Some("enabled"))
        .and_then(|m| m.get("primary"))
        .and_then(|p| p.as_bool()))
}

/// This site's mirroring status for an image (`rbd mirror image status pool/image --format json`),
/// as written by the local `rbd-mirror` daemon, plus what each peer site reports.
pub async fn rbd_mirror_image_status(
    pool: &str,
    image: &str,
) -> Result<serde_json::Value, DriverError> {
    let spec = format!("{pool}/{image}");
    rbd_cmd(&["mirror", "image", "status", &spec]).await
}

/// Mirroring mode of an RBD image (`journal` / `snapshot`) from `rbd info`, `None` when mirroring
/// is not enabled on it.
pub async fn rbd_mirror_mode(pool: &str, image: &str) -> Result<Option<String>, DriverError> {
    let spec = format!("{pool}/{image}");
    let v = rbd_cmd(&["info", &spec]).await?;
    Ok(v.get("mirroring")
        .filter(|m| m.get("state").and_then(|s| s.as_str()) == Some("enabled"))
        .and_then(|m| m.get("mode"))
        .and_then(|m| m.as_str())
        .map(str::to_string))
}

/// Whether every peer `rbd-mirror` client registered on this image's local journal has committed
/// up to the local client's position (`rbd journal status`). Meaningful on the site that wrote the
/// journal, i.e. the primary or the copy just demoted: `Some(true)` there means the peer replayed
/// everything, including the demotion. `None` when the journal is empty here or has no peer.
pub async fn rbd_journal_peers_replayed(
    pool: &str,
    image: &str,
) -> Result<Option<bool>, DriverError> {
    let v = rbd_cmd(&["journal", "status", "--pool", pool, "--image", image]).await?;
    Ok(journal_peers_replayed(&v))
}

/// [`rbd_journal_peers_replayed`] on an already-parsed `rbd journal status --format json`.
pub fn journal_peers_replayed(status: &serde_json::Value) -> Option<bool> {
    let clients = status.get("registered_clients")?.as_array()?;
    let positions = |c: &serde_json::Value| -> Vec<(u64, u64, u64)> {
        let mut p: Vec<(u64, u64, u64)> = c
            .pointer("/commit_position/object_positions")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .map(|o| {
                        let n = |k: &str| o.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                        (n("object_number"), n("tag_tid"), n("entry_tid"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        p.sort_unstable();
        p
    };
    let is_local = |c: &&serde_json::Value| c.get("id").and_then(|i| i.as_str()) == Some("");
    let local = positions(clients.iter().find(is_local)?);
    if local.is_empty() {
        return None;
    }
    let mut peers = clients.iter().filter(|c| !is_local(c)).peekable();
    peers.peek()?;
    Some(peers.all(|c| positions(c) == local))
}

/// Why a non-forced promote of this site's non-primary copy would not be a clean failback, given
/// its `rbd mirror image status` JSON and mirroring mode; `None` when it is safe.
///
/// Snapshot mode: Ceph accepts a non-forced promote whenever the newest local mirror snapshot is a
/// demotion, including when this site never ran `rbd-mirror` and so never replayed the peer's later
/// writes (one-way topologies). Only the local daemon reporting `up` and "remote image demoted"
/// shows that it has caught up with the peer's demotion.
///
/// Journal mode: once the peer is demoted this site reports "remote image is not primary" whether or
/// not it replayed the peer's journal, so the status can't prove it. `peer_replayed` is the
/// operator's confirmation that the peer's [`rbd_journal_peers_replayed`] was `true`.
pub fn mirror_promote_blocker(
    status: &serde_json::Value,
    mode: &str,
    peer_replayed: bool,
) -> Option<String> {
    let state = status.get("state").and_then(|s| s.as_str()).unwrap_or("");
    let description = status
        .get("description")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    if !state.starts_with("up+") {
        return Some(format!(
            "no rbd-mirror daemon is replaying this image here (state '{state}'); this site may \
             be missing the peer's writes"
        ));
    }
    if mode == "journal" {
        if !(description.contains("remote image is not primary")
            || description.contains("remote image demoted"))
        {
            return Some(format!(
                "the peer has not demoted its copy, or this site is still replaying its journal \
                 (state '{state}', '{description}')"
            ));
        }
        if !peer_replayed {
            return Some(
                "journal mode: this site's status can't show whether it replayed the peer's \
                 journal; confirm the peer's GET /dr/mirrors/{id}/status reports \
                 journal_peers_replayed: true, then promote with peer_replayed=1"
                    .to_string(),
            );
        }
        return None;
    }
    if !description.contains("remote image demoted") {
        return Some(format!(
            "the peer has not demoted its copy, or this site has not replayed it yet \
             (state '{state}', '{description}')"
        ));
    }
    None
}

/// Actual used (allocated) bytes of every image in a pool (`rbd du --pool <pool> --format json`),
/// as `(image_name, used_size)` pairs. Used to enrich `rbd ls -l` (which has no `used_size` field).
/// The pool goes in `--pool`: a bare positional is an image spec in the default pool (`rbd`).
pub async fn rbd_du_pool(pool: &str) -> Result<Vec<(String, i64)>, DriverError> {
    let v = rbd_cmd(&["du", "--pool", pool]).await?;
    Ok(parse_du_images(&v))
}

/// Per-image `(name, used_size)` from `rbd du --format json`. Squid names the image `name`;
/// `image` is what `rbd ls -l` uses, so accept both.
fn parse_du_images(v: &serde_json::Value) -> Vec<(String, i64)> {
    v.get("images")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|img| {
                    let name = img
                        .get("name")
                        .or_else(|| img.get("image"))
                        .and_then(|n| n.as_str())?
                        .to_string();
                    let used = img.get("used_size").and_then(|u| u.as_i64())?;
                    Some((name, used))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Actual used (allocated) bytes of an RBD image (`rbd du pool/image --format json`).
pub async fn rbd_du_image(pool: &str, image: &str) -> Result<i64, DriverError> {
    let spec = format!("{pool}/{image}");
    let v = rbd_cmd(&["du", &spec]).await?;
    v.get("images")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|img| img.get("used_size"))
        .and_then(|u| u.as_i64())
        .ok_or_else(|| DriverError::Parse(format!("rbd du {spec}: no used_size")))
}

/// Protect a snapshot so it can be used as a clone parent (`rbd snap protect`).
pub async fn rbd_snap_protect(pool: &str, image: &str, snap: &str) -> Result<(), DriverError> {
    snap_op(&["snap", "protect"], pool, image, snap).await
}

/// Unprotect a snapshot (`rbd snap unprotect`); best-effort on "not protected".
pub async fn rbd_snap_unprotect(pool: &str, image: &str, snap: &str) -> Result<(), DriverError> {
    let spec = format!("{pool}/{image}@{snap}");
    let output = cmd("rbd")
        .args(["snap", "unprotect", &spec])
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not protected")
            || stderr.contains("already unprotected")
            || stderr.contains("does not exist")
        {
            return Ok(());
        }
        return Err(DriverError::Backend(format!(
            "rbd snap unprotect {spec}: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

/// Clone a protected snapshot into a new COW image (`rbd clone parent@snap clone`).
pub async fn rbd_clone(
    parent_pool: &str,
    parent_image: &str,
    snap: &str,
    clone_pool: &str,
    clone_image: &str,
) -> Result<(), DriverError> {
    let src = format!("{parent_pool}/{parent_image}@{snap}");
    let dst = format!("{clone_pool}/{clone_image}");
    let output = cmd("rbd")
        .args(["clone", &src, &dst])
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!(
            "rbd clone {src} {dst}: {stderr}"
        )));
    }
    Ok(())
}

async fn snap_op(op: &[&str], pool: &str, image: &str, snap: &str) -> Result<(), DriverError> {
    let spec = format!("{pool}/{image}@{snap}");
    let mut args: Vec<&str> = op.to_vec();
    args.push(&spec);
    let output = cmd("rbd")
        .args(&args)
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!(
            "rbd {} {spec}: {stderr}",
            op.join(" ")
        )));
    }
    Ok(())
}

/// List RBD image names in a pool (`rbd ls pool --format json`).
pub async fn rbd_list(pool: &str) -> Result<Vec<String>, DriverError> {
    let v = rbd_cmd(&["ls", pool]).await?;
    Ok(v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default())
}

/// Remove an RBD snapshot `pool/image@snap` (best-effort; ignores "not found").
pub async fn rbd_snap_rm(pool: &str, image: &str, snap: &str) -> Result<(), DriverError> {
    let spec = format!("{pool}/{image}@{snap}");
    let output = cmd("rbd")
        .args(["snap", "rm", &spec])
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such file") || stderr.contains("does not exist") {
            return Ok(());
        }
        return Err(DriverError::Backend(format!(
            "rbd snap rm {spec}: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

/// Export an RBD snapshot as an incremental diff stream (`rbd export-diff pool/image@snap -`).
/// For a fresh/sparse image this is small; the caller must cap the size it buffers.
pub async fn rbd_export_diff(pool: &str, image: &str, snap: &str) -> Result<Vec<u8>, DriverError> {
    let spec = format!("{pool}/{image}@{snap}");
    let output = cmd("rbd")
        .args(["export-diff", &spec, "-"])
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!(
            "rbd export-diff {spec}: {stderr}"
        )));
    }
    Ok(output.stdout)
}

/// Spawn `rbd export-diff pool/image@snap -` with stdout piped, for streaming the diff elsewhere
/// (e.g. straight into an S3 multipart upload) without buffering it in memory. The caller takes
/// `child.stdout`, streams it, then `wait()`s and checks the exit status.
pub fn rbd_export_diff_child(
    pool: &str,
    image: &str,
    snap: &str,
) -> Result<tokio::process::Child, DriverError> {
    let spec = format!("{pool}/{image}@{snap}");
    cmd("rbd")
        .args(["export-diff", &spec, "-"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))
}

/// Spawn `rbd export-diff pool/image -` of the image head (no snapshot), stdout piped. Read-only;
/// the caller reads at most what it needs and then kills the child.
pub fn rbd_export_diff_head_child(pool: &str, image: &str) -> Result<tokio::process::Child, DriverError> {
    let spec = format!("{pool}/{image}");
    cmd("rbd")
        .args(["export-diff", &spec, "-"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))
}

/// Spawn `rbd import-diff - pool/image` with stdin piped, for streaming a diff into it (e.g. from
/// an S3 download) without buffering. The caller writes to `child.stdin`, drops it, then `wait()`s.
pub fn rbd_import_diff_child(
    pool: &str,
    image: &str,
) -> Result<tokio::process::Child, DriverError> {
    let spec = format!("{pool}/{image}");
    cmd("rbd")
        .args(["import-diff", "-", &spec])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))
}

/// Apply an RBD diff stream (from `rbd export-diff`) into an existing image via stdin
/// (`rbd import-diff - pool/image`). Used by restore-from-data.
pub async fn rbd_import_diff(pool: &str, image: &str, data: Vec<u8>) -> Result<(), DriverError> {
    use tokio::io::AsyncWriteExt;
    let spec = format!("{pool}/{image}");
    let mut child = cmd("rbd")
        .args(["import-diff", "-", &spec])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| DriverError::Backend("rbd import-diff: no stdin".into()))?;
        stdin
            .write_all(&data)
            .await
            .map_err(|e| DriverError::Backend(format!("rbd import-diff write: {e}")))?;
        let _ = stdin.shutdown().await;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| DriverError::Backend(format!("rbd import-diff wait: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!(
            "rbd import-diff {spec}: {stderr}"
        )));
    }
    Ok(())
}

/// Run `rbd <args...>` for its side effect; stderr becomes the error.
async fn run_rbd(args: &[&str]) -> Result<(), DriverError> {
    let output = cmd("rbd")
        .args(args)
        .output()
        .await
        .map_err(|e| DriverError::Unreachable(format!("failed to spawn `rbd`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!(
            "rbd {}: {stderr}",
            args.join(" ")
        )));
    }
    Ok(())
}

/// Pool-level mirroring configuration (`rbd mirror pool info <pool> --format json`): `mode` is
/// `disabled`, `image` (per-image opt-in) or `pool` (every journaling image is mirrored).
pub async fn rbd_mirror_pool_info(pool: &str) -> Result<serde_json::Value, DriverError> {
    rbd_cmd(&["mirror", "pool", "info", pool]).await
}

/// Set a pool's mirroring mode: `rbd mirror pool enable <pool> image|pool`. On Rook the
/// CephBlockPool's `spec.mirroring.mode` is the source of truth and is re-applied on reconcile.
pub async fn rbd_mirror_pool_enable(pool: &str, mode: &str) -> Result<(), DriverError> {
    if !matches!(mode, "image" | "pool") {
        return Err(DriverError::Backend(format!(
            "pool mirror mode must be image or pool, got {mode}"
        )));
    }
    run_rbd(&["mirror", "pool", "enable", pool, mode]).await
}

/// Enable the `journaling` feature (which needs `exclusive-lock`) unless the image already has it.
/// In a pool-mode pool this is what puts an image under mirroring.
pub async fn rbd_enable_journaling(pool: &str, image: &str) -> Result<(), DriverError> {
    let spec = format!("{pool}/{image}");
    let info = rbd_cmd(&["info", &spec]).await?;
    let has = |f: &str| {
        info.get("features")
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(f)))
    };
    if has("journaling") {
        return Ok(());
    }
    if !has("exclusive-lock") {
        run_rbd(&["feature", "enable", &spec, "exclusive-lock"]).await?;
    }
    run_rbd(&["feature", "enable", &spec, "journaling"]).await
}

/// Take a mirror snapshot of a snapshot-mode primary now (`rbd mirror image snapshot`), so the
/// peer syncs everything up to this point, including user and group snapshots taken before it.
pub async fn rbd_mirror_image_snapshot(pool: &str, image: &str) -> Result<(), DriverError> {
    let spec = format!("{pool}/{image}");
    run_rbd(&["mirror", "image", "snapshot", &spec]).await
}

/// `rbd group create <pool>/<group>`.
pub async fn rbd_group_create(pool: &str, group: &str) -> Result<(), DriverError> {
    run_rbd(&["group", "create", &format!("{pool}/{group}")]).await
}

/// `rbd group remove <pool>/<group>`: removes the group, never its images.
pub async fn rbd_group_remove(pool: &str, group: &str) -> Result<(), DriverError> {
    run_rbd(&["group", "remove", &format!("{pool}/{group}")]).await
}

/// `rbd group image add <pool>/<group> <image_pool>/<image>`.
pub async fn rbd_group_image_add(
    pool: &str,
    group: &str,
    image_pool: &str,
    image: &str,
) -> Result<(), DriverError> {
    run_rbd(&[
        "group",
        "image",
        "add",
        &format!("{pool}/{group}"),
        &format!("{image_pool}/{image}"),
    ])
    .await
}

/// `rbd group image remove <pool>/<group> <image_pool>/<image>`.
pub async fn rbd_group_image_remove(
    pool: &str,
    group: &str,
    image_pool: &str,
    image: &str,
) -> Result<(), DriverError> {
    run_rbd(&[
        "group",
        "image",
        "remove",
        &format!("{pool}/{group}"),
        &format!("{image_pool}/{image}"),
    ])
    .await
}

/// `rbd group snap create <pool>/<group>@<snap>`: one crash-consistent point across every member
/// (librbd quiesces and blocks writes to all of them while the member snapshots are taken).
pub async fn rbd_group_snap_create(pool: &str, group: &str, snap: &str) -> Result<(), DriverError> {
    run_rbd(&["group", "snap", "create", &format!("{pool}/{group}@{snap}")]).await
}

/// `rbd group snap remove <pool>/<group>@<snap>`.
pub async fn rbd_group_snap_remove(pool: &str, group: &str, snap: &str) -> Result<(), DriverError> {
    run_rbd(&["group", "snap", "remove", &format!("{pool}/{group}@{snap}")]).await
}

/// `rbd group snap rollback <pool>/<group>@<snap>`: every member back to that point (destructive;
/// the images must not be in use).
pub async fn rbd_group_snap_rollback(
    pool: &str,
    group: &str,
    snap: &str,
) -> Result<(), DriverError> {
    run_rbd(&[
        "group",
        "snap",
        "rollback",
        &format!("{pool}/{group}@{snap}"),
    ])
    .await
}

/// `rbd group snap list <pool>/<group> --format json`.
pub async fn rbd_group_snap_list(
    pool: &str,
    group: &str,
) -> Result<serde_json::Value, DriverError> {
    rbd_cmd(&["group", "snap", "list", &format!("{pool}/{group}")]).await
}

/// `rbd group image list <pool>/<group> --format json`.
pub async fn rbd_group_image_list(
    pool: &str,
    group: &str,
) -> Result<serde_json::Value, DriverError> {
    rbd_cmd(&["group", "image", "list", &format!("{pool}/{group}")]).await
}

async fn run_json(bin: &str, args: &[&str]) -> Result<serde_json::Value, DriverError> {
    let output = cmd(bin)
        .args(args)
        .arg("--format")
        .arg("json")
        .output()
        .await
        .map_err(|e| {
            // e.g. binary missing, or no permission — treat as backend unreachable.
            DriverError::Unreachable(format!("failed to spawn `{bin}`: {e}"))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(DriverError::Backend(format!(
            "`{bin} {}` failed: {stderr}",
            args.join(" ")
        )));
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|e| DriverError::Parse(format!("`{bin}` json: {e}")))
}

#[cfg(test)]
mod tests {
    use super::{
        interpret_ok_to_stop, journal_peers_replayed, mirror_promote_blocker, parse_du_images,
    };
    use serde_json::json;

    #[test]
    fn du_images_accept_the_squid_name_key_and_the_older_image_key() {
        // Captured from `rbd du --pool rbd-nvme-prod --format json` on Ceph 19.2.3.
        let squid = json!({"images":[
            {"name":"csi-vol-8ae5","id":"15295d","provisioned_size":1073741824,"used_size":50331648},
            {"name":"dedup-live","id":"132379","provisioned_size":1048576,"used_size":1048576}],
            "total_provisioned_size":1074790400,"total_used_size":51380224});
        assert_eq!(
            parse_du_images(&squid),
            vec![("csi-vol-8ae5".to_string(), 50331648), ("dedup-live".to_string(), 1048576)]
        );
        let older = json!({"images":[{"image":"a","used_size":7}]});
        assert_eq!(parse_du_images(&older), vec![("a".to_string(), 7)]);
        assert!(parse_du_images(&json!({})).is_empty());
    }

    #[test]
    fn ok_to_stop_success_carries_the_flag_and_cephs_detail() {
        let out = br#"{"ok_to_stop":true,"osds":[1],"num_ok_pgs":12,"num_not_ok_pgs":0}"#;
        let v = interpret_ok_to_stop(true, out, "").unwrap();
        assert_eq!(v["ok_to_stop"], true);
        assert_eq!(v["num_ok_pgs"], 12);
        // Older Ceph omits the flag on success; it is added.
        assert_eq!(interpret_ok_to_stop(true, b"{}", "").unwrap()["ok_to_stop"], true);
        assert_eq!(interpret_ok_to_stop(true, b"", "").unwrap()["ok_to_stop"], true);
    }

    #[test]
    fn ok_to_stop_refusal_is_a_result_not_an_error() {
        let out = br#"{"ok_to_stop":false,"osds":[2],"num_ok_pgs":4,"num_not_ok_pgs":9,"bad_become_inactive":["1.a"]}"#;
        let v = interpret_ok_to_stop(false, out, "Error EBUSY: unsafe to stop osd(s)\n").unwrap();
        assert_eq!(v["ok_to_stop"], false);
        assert_eq!(v["num_not_ok_pgs"], 9);
        assert_eq!(v["reason"], "Error EBUSY: unsafe to stop osd(s)");
        // Forced false even if the JSON said otherwise on a failing exit.
        let odd = interpret_ok_to_stop(false, br#"{"ok_to_stop":true}"#, "").unwrap();
        assert_eq!(odd["ok_to_stop"], false);
    }

    #[test]
    fn ok_to_stop_failure_without_json_is_an_error_with_cephs_message() {
        let e = interpret_ok_to_stop(false, b"", "Error ENOENT: osd.99 does not exist\n").unwrap_err();
        assert!(e.to_string().contains("osd.99 does not exist"), "{e}");
        assert!(interpret_ok_to_stop(false, b"not json", "boom").is_err());
        assert!(interpret_ok_to_stop(false, b"[1,2]", "boom").is_err());
    }

    #[test]
    fn promote_is_clean_only_after_replaying_the_peer_demotion() {
        let ready = json!({ "state": "up+unknown", "description": "remote image demoted" });
        assert_eq!(mirror_promote_blocker(&ready, "snapshot", false), None);

        let replaying = json!({ "state": "up+replaying", "description": "replaying, {}" });
        assert!(mirror_promote_blocker(&replaying, "snapshot", false)
            .unwrap()
            .contains("not demoted"));

        let no_daemon = json!({ "state": "down+unknown", "description": "remote image demoted" });
        assert!(mirror_promote_blocker(&no_daemon, "snapshot", false)
            .unwrap()
            .contains("no rbd-mirror daemon"));

        assert!(mirror_promote_blocker(&json!({}), "snapshot", false).is_some());

        let not_primary =
            json!({ "state": "up+unknown", "description": "remote image is not primary" });
        assert!(mirror_promote_blocker(&not_primary, "snapshot", true).is_some());
    }

    #[test]
    fn journal_promote_needs_the_peer_confirmation() {
        let caught_up =
            json!({ "state": "up+unknown", "description": "remote image is not primary" });
        assert!(mirror_promote_blocker(&caught_up, "journal", false)
            .unwrap()
            .contains("peer_replayed=1"));
        assert_eq!(mirror_promote_blocker(&caught_up, "journal", true), None);

        let replaying = json!({ "state": "up+replaying", "description": "replaying, {}" });
        assert!(mirror_promote_blocker(&replaying, "journal", true).is_some());
        let down = json!({ "state": "down+stopped", "description": "remote image is not primary" });
        assert!(mirror_promote_blocker(&down, "journal", true).is_some());
    }

    #[test]
    fn journal_peers_replayed_compares_commit_positions() {
        let pos = |e: u64| {
            json!({ "object_positions": [
                { "object_number": 0, "tag_tid": 5, "entry_tid": 0 },
                { "object_number": 3, "tag_tid": 3, "entry_tid": e },
            ]})
        };
        let status = |peer: u64| {
            json!({ "registered_clients": [
                { "id": "", "commit_position": pos(523) },
                { "id": "612bd1f5", "commit_position": pos(peer) },
            ]})
        };
        assert_eq!(journal_peers_replayed(&status(523)), Some(true));
        assert_eq!(journal_peers_replayed(&status(111)), Some(false));

        let empty = json!({ "registered_clients": [
            { "id": "", "commit_position": { "object_positions": [] } },
            { "id": "db204098", "commit_position": { "object_positions": [] } },
        ]});
        assert_eq!(journal_peers_replayed(&empty), None);
        let no_peer = json!({ "registered_clients": [{ "id": "", "commit_position": pos(1) }] });
        assert_eq!(journal_peers_replayed(&no_peer), None);
    }
}
