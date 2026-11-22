// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The CSI node service: each publish runs one `atlas-native-mount` process on the pod's target
//! path, kept as a child of this plugin until the volume is unpublished. The mounts live as long
//! as the plugin process, so restarting the plugin pod breaks the volumes it published (pods
//! see `ENOTCONN` until they are restarted; a republish of the same target remounts it).

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use tonic::{Request, Response, Status};

use crate::{
    mountinfo::is_mount_point,
    proto::{
        node_server::Node, node_service_capability, volume_capability, volume_usage,
        NodeGetCapabilitiesRequest, NodeGetCapabilitiesResponse, NodeGetInfoRequest,
        NodeGetInfoResponse, NodeGetVolumeStatsRequest, NodeGetVolumeStatsResponse,
        NodePublishVolumeRequest, NodePublishVolumeResponse, NodeServiceCapability,
        NodeUnpublishVolumeRequest, NodeUnpublishVolumeResponse, VolumeCondition, VolumeUsage,
    },
};

pub struct NodeConfig {
    pub node_id: String,
    /// Path of the `atlas-native-mount` binary.
    pub mount_binary: PathBuf,
    /// Connection arguments for every mount (`--endpoint`, `--token-file`, ...): file paths only,
    /// so no secret appears on a command line.
    pub mount_args: Vec<String>,
    /// How long a mount may take to appear before the publish fails.
    pub mount_timeout: Duration,
}

pub struct NodeService {
    cfg: Arc<NodeConfig>,
    children: Arc<Mutex<HashMap<PathBuf, Child>>>,
    /// Targets with a publish or unpublish in flight.
    busy: Arc<Mutex<HashSet<PathBuf>>>,
}

impl NodeService {
    pub fn new(cfg: NodeConfig) -> Self {
        Self {
            cfg: Arc::new(cfg),
            children: Default::default(),
            busy: Default::default(),
        }
    }

    async fn exclusive<T: Send + 'static>(
        &self,
        target: PathBuf,
        f: impl FnOnce(&NodeConfig, &Mutex<HashMap<PathBuf, Child>>, &Path) -> Result<T, Status>
            + Send
            + 'static,
    ) -> Result<T, Status> {
        if !self.busy.lock().unwrap().insert(target.clone()) {
            return Err(Status::aborted(format!(
                "an operation on {} is in progress",
                target.display()
            )));
        }
        let (cfg, children, busy) = (self.cfg.clone(), self.children.clone(), self.busy.clone());
        tokio::task::spawn_blocking(move || {
            let r = f(&cfg, &children, &target);
            busy.lock().unwrap().remove(&target);
            r
        })
        .await
        .map_err(|e| Status::internal(format!("node task: {e}")))?
    }
}

/// Mount flags a volume may carry (StorageClass `mountOptions`), as `atlas-native-mount`
/// arguments. Anything else is refused, so a mount option can't redirect the connection.
pub fn mount_flag_args(flags: &[String]) -> Result<Vec<String>, Status> {
    const SWITCHES: &[&str] = &["direct-reads", "cache-leases", "acl"];
    const VALUED: &[&str] = &[
        "ttl-ms",
        "writeback-bytes",
        "writeback-parallel",
        "readahead-bytes",
        "max-io-bytes",
        "fuse-threads",
        "session-ttl-ms",
        "retry-secs",
    ];
    let mut args = Vec::new();
    for f in flags.iter().flat_map(|f| f.split(',')).map(str::trim) {
        match f.split_once('=') {
            _ if f.is_empty() || f == "rw" => {}
            None if f == "ro" => args.push("--read-only".into()),
            None if SWITCHES.contains(&f) => args.push(format!("--{f}")),
            Some((k, v))
                if VALUED.contains(&k)
                    && !v.is_empty()
                    && v.bytes().all(|b| b.is_ascii_digit()) =>
            {
                args.push(format!("--{k}"));
                args.push(v.into());
            }
            _ => {
                return Err(Status::invalid_argument(format!(
                    "unsupported mount option {f:?} (supported: ro, {}, {}=<n>)",
                    SWITCHES.join(", "),
                    VALUED.join("=<n>, ")
                )))
            }
        }
    }
    Ok(args)
}

#[derive(PartialEq)]
enum State {
    Absent,
    Healthy,
    /// Listed as a mount but unusable (its FUSE process is gone).
    Broken,
}

fn state(target: &Path) -> Result<State, Status> {
    let mounted =
        is_mount_point(target).map_err(|e| Status::internal(format!("reading mountinfo: {e}")))?;
    Ok(match (mounted, std::fs::metadata(target)) {
        (false, _) => State::Absent,
        (true, Ok(_)) => State::Healthy,
        (true, Err(_)) => State::Broken,
    })
}

/// Unmounts with the syscall rather than `fusermount3 -u`: the plugin runs as root, and hosts
/// may confine `fusermount3` (Ubuntu's AppArmor profile refuses unmounts under the kubelet root).
#[cfg(target_os = "linux")]
fn umount(target: &Path, flags: libc::c_int) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(target.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `path` is a valid NUL-terminated string that outlives the call.
    match unsafe { libc::umount2(path.as_ptr(), flags) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

#[cfg(not(target_os = "linux"))]
fn umount(_target: &Path, _flags: i32) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(target_os = "linux")]
const MNT_DETACH: i32 = libc::MNT_DETACH;
#[cfg(not(target_os = "linux"))]
const MNT_DETACH: i32 = 0;

fn unmount(target: &Path) -> Result<(), Status> {
    let mut last = None;
    // A lazy unmount is the fallback for a mount still held open after its pod is gone.
    for flags in [0, MNT_DETACH] {
        match umount(target, flags) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        if state(target)? == State::Absent {
            return Ok(());
        }
    }
    Err(Status::internal(format!(
        "unmounting {}: {}",
        target.display(),
        last.map_or_else(String::new, |e| e.to_string())
    )))
}

/// Waits briefly for a mount process to exit after its unmount, then kills it.
fn reap(mut child: Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn publish(
    cfg: &NodeConfig,
    children: &Mutex<HashMap<PathBuf, Child>>,
    target: &Path,
    volume_id: &str,
    read_only: bool,
    flags: Vec<String>,
) -> Result<(), Status> {
    match state(target)? {
        State::Healthy => return Ok(()),
        State::Broken => unmount(target)?,
        State::Absent => {}
    }
    if let Some(old) = children.lock().unwrap().remove(target) {
        reap(old);
    }
    std::fs::create_dir_all(target)
        .map_err(|e| Status::internal(format!("creating {}: {e}", target.display())))?;
    let mut cmd = Command::new(&cfg.mount_binary);
    cmd.args(&cfg.mount_args)
        .args(["--fs", volume_id, "--allow-other"])
        .args(&flags)
        .stdin(Stdio::null());
    if read_only {
        cmd.arg("--read-only");
    }
    cmd.arg(target);
    let mut child = cmd
        .spawn()
        .map_err(|e| Status::internal(format!("{}: {e}", cfg.mount_binary.display())))?;
    let deadline = Instant::now() + cfg.mount_timeout;
    loop {
        if state(target)? == State::Healthy {
            children.lock().unwrap().insert(target.to_path_buf(), child);
            return Ok(());
        }
        if let Ok(Some(st)) = child.try_wait() {
            return Err(Status::internal(format!(
                "atlas-native-mount for volume {volume_id} exited ({st}) before mounting; see the plugin log"
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Status::deadline_exceeded(format!(
                "volume {volume_id} was not mounted within {:?}",
                cfg.mount_timeout
            )));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn unpublish(
    _cfg: &NodeConfig,
    children: &Mutex<HashMap<PathBuf, Child>>,
    target: &Path,
) -> Result<(), Status> {
    if state(target)? != State::Absent {
        unmount(target)?;
    }
    if let Some(child) = children.lock().unwrap().remove(target) {
        reap(child);
    }
    match std::fs::remove_dir(target) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(Status::internal(format!(
            "removing {}: {e}",
            target.display()
        ))),
        _ => Ok(()),
    }
}

/// How long a stats request waits on a mount's `statfs` before reporting the volume abnormal.
const STATS_TIMEOUT: Duration = Duration::from_secs(10);

/// Bytes and inodes of the filesystem mounted at `path` (the FUSE client reports a quota as the
/// size, so a capacity-enforced volume shows its PVC size).
fn usage(path: &Path) -> std::io::Result<Vec<VolumeUsage>> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `c` is NUL-terminated and outlives the call; `st` is written by statvfs.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let frsize = st.f_frsize as u64;
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let (blocks, bfree, bavail) = (st.f_blocks as u64, st.f_bfree as u64, st.f_bavail as u64);
    let (files, ffree, favail) = (st.f_files as u64, st.f_ffree as u64, st.f_favail as u64);
    Ok(vec![
        VolumeUsage {
            available: n(bavail.saturating_mul(frsize)),
            total: n(blocks.saturating_mul(frsize)),
            used: n(blocks.saturating_sub(bfree).saturating_mul(frsize)),
            unit: volume_usage::Unit::Bytes as i32,
        },
        VolumeUsage {
            available: n(favail),
            total: n(files),
            used: n(files.saturating_sub(ffree)),
            unit: volume_usage::Unit::Inodes as i32,
        },
    ])
}

fn node_rpc(t: node_service_capability::rpc::Type) -> NodeServiceCapability {
    NodeServiceCapability {
        r#type: Some(node_service_capability::Type::Rpc(
            node_service_capability::Rpc { r#type: t as i32 },
        )),
    }
}

fn abnormal(message: String) -> NodeGetVolumeStatsResponse {
    NodeGetVolumeStatsResponse {
        usage: Vec::new(),
        volume_condition: Some(VolumeCondition {
            abnormal: true,
            message,
        }),
    }
}

#[tonic::async_trait]
impl Node for NodeService {
    async fn node_publish_volume(
        &self,
        req: Request<NodePublishVolumeRequest>,
    ) -> Result<Response<NodePublishVolumeResponse>, Status> {
        let r = req.into_inner();
        if r.volume_id.is_empty() {
            return Err(Status::invalid_argument("volume_id is required"));
        }
        if r.target_path.is_empty() {
            return Err(Status::invalid_argument("target_path is required"));
        }
        let cap = r
            .volume_capability
            .ok_or_else(|| Status::invalid_argument("volume_capability is required"))?;
        let flags = match cap.access_type {
            Some(volume_capability::AccessType::Mount(m)) => mount_flag_args(&m.mount_flags)?,
            _ => {
                return Err(Status::invalid_argument(
                    "only filesystem (mount) volumes are supported",
                ))
            }
        };
        if !crate::controller::valid_id(&r.volume_id) {
            return Err(Status::not_found(format!("volume {}", r.volume_id)));
        }
        let (id, ro) = (r.volume_id, r.readonly);
        self.exclusive(
            PathBuf::from(r.target_path),
            move |cfg, children, target| publish(cfg, children, target, &id, ro, flags),
        )
        .await?;
        Ok(Response::new(NodePublishVolumeResponse {}))
    }

    async fn node_unpublish_volume(
        &self,
        req: Request<NodeUnpublishVolumeRequest>,
    ) -> Result<Response<NodeUnpublishVolumeResponse>, Status> {
        let r = req.into_inner();
        if r.volume_id.is_empty() {
            return Err(Status::invalid_argument("volume_id is required"));
        }
        if r.target_path.is_empty() {
            return Err(Status::invalid_argument("target_path is required"));
        }
        self.exclusive(PathBuf::from(r.target_path), unpublish)
            .await?;
        Ok(Response::new(NodeUnpublishVolumeResponse {}))
    }

    async fn node_get_capabilities(
        &self,
        _: Request<NodeGetCapabilitiesRequest>,
    ) -> Result<Response<NodeGetCapabilitiesResponse>, Status> {
        use node_service_capability::rpc::Type;
        Ok(Response::new(NodeGetCapabilitiesResponse {
            capabilities: vec![
                node_rpc(Type::GetVolumeStats),
                node_rpc(Type::VolumeCondition),
            ],
        }))
    }

    async fn node_get_volume_stats(
        &self,
        req: Request<NodeGetVolumeStatsRequest>,
    ) -> Result<Response<NodeGetVolumeStatsResponse>, Status> {
        let r = req.into_inner();
        if r.volume_id.is_empty() {
            return Err(Status::invalid_argument("volume_id is required"));
        }
        if r.volume_path.is_empty() {
            return Err(Status::invalid_argument("volume_path is required"));
        }
        let (id, path) = (r.volume_id, PathBuf::from(r.volume_path));
        // Even finding out whether it is mounted touches the mount, which can hang.
        let probe = tokio::task::spawn_blocking(move || {
            match state(&path)? {
            State::Absent => Err(Status::not_found(format!(
                "volume {id} is not mounted at {}",
                path.display()
            ))),
            State::Broken => Ok(Err(
                "the volume's FUSE client is gone (plugin restarted?); restart the pod to remount it"
                    .to_string(),
            )),
            State::Healthy => Ok(usage(&path).map_err(|e| format!("statfs: {e}"))),
        }
        });
        let resp = match tokio::time::timeout(STATS_TIMEOUT, probe).await {
            Err(_) => abnormal(format!(
                "the mount did not answer within {STATS_TIMEOUT:?}; the cluster may be unreachable"
            )),
            Ok(Err(e)) => return Err(Status::internal(format!("stats task: {e}"))),
            Ok(Ok(probed)) => match probed? {
                Ok(usage) => NodeGetVolumeStatsResponse {
                    usage,
                    volume_condition: Some(VolumeCondition {
                        abnormal: false,
                        message: String::new(),
                    }),
                },
                Err(message) => abnormal(message),
            },
        };
        Ok(Response::new(resp))
    }

    async fn node_get_info(
        &self,
        _: Request<NodeGetInfoRequest>,
    ) -> Result<Response<NodeGetInfoResponse>, Status> {
        Ok(Response::new(NodeGetInfoResponse {
            node_id: self.cfg.node_id.clone(),
            max_volumes_per_node: 0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_options_map_to_whitelisted_flags() {
        let f = |s: &[&str]| mount_flag_args(&s.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            f(&["ro,cache-leases", "writeback-parallel=8", "rw"]).unwrap(),
            vec!["--read-only", "--cache-leases", "--writeback-parallel", "8"]
        );
        assert!(f(&["endpoint=http://evil"]).is_err());
        assert!(f(&["token-file=/etc/shadow"]).is_err());
        assert!(f(&["writeback-parallel=-1"]).is_err());
        assert!(f(&["writeback-parallel="]).is_err());
        assert!(f(&["noatime"]).is_err());
        assert_eq!(f(&["acl"]).unwrap(), vec!["--acl"]);
    }

    #[test]
    fn usage_reads_bytes_and_inodes_of_a_mount() {
        let td = tempfile::tempdir().unwrap();
        let u = usage(td.path()).unwrap();
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].unit, volume_usage::Unit::Bytes as i32);
        assert!(u[0].total > 0 && u[0].used <= u[0].total && u[0].available <= u[0].total);
        assert_eq!(u[1].unit, volume_usage::Unit::Inodes as i32);
        assert!(usage(&td.path().join("missing")).is_err());
    }
}
