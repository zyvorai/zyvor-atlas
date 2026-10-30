// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Networked data plane. A [`DataNodeServer`] serves one or more local devices
//! ([`crate::device::FileDevice`] or [`crate::raw::RawDevice`]) over TCP and a [`RemoteDevice`]
//! is the engine-side client for one of them, so replicas land on other hosts' disks.
//!
//! Wire format, both directions: a 4-byte big-endian header length, a JSON header, then the raw
//! payload the header announces (write data in requests, read data in responses). Without TLS
//! there is no authentication or encryption; bind to a private storage network only. With a
//! [`TlsIdentity`] connections are mutual TLS against the cluster CA: clients must present a
//! CA-signed certificate, and verify the node's certificate against its node id.
//!
//! Every write carries a fence (the writer's Raft term) and the metadata group it writes for.
//! The node durably records the highest fence it has accepted from each group and rejects lower
//! ones, so a deposed leader that has not noticed yet cannot overwrite a range the new leader of
//! its group has reallocated. Groups have independent terms and never share a range.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use rustls::{pki_types::ServerName, ClientConfig, ServerConfig};
use serde::{Deserialize, Serialize};

use crate::{
    device::{BlockStore, FileDevice},
    durable,
    engine::NativeError,
    metrics::PromText,
    tls::{self, Conn, TlsIdentity},
};

const MAX_HEADER: usize = 64 << 10;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Payloads up to this size are sent in the same write as their header.
const SMALL_PAYLOAD: usize = 16 << 10;
/// Largest payload accepted in either direction.
pub const MAX_PAYLOAD: u64 = 256 << 20;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Append {
        fence: u64,
        len: u64,
        #[serde(default, skip_serializing_if = "is_zero")]
        device: usize,
        #[serde(default, skip_serializing_if = "is_zero_group")]
        group: u32,
    },
    WriteAt {
        fence: u64,
        offset: u64,
        len: u64,
        #[serde(default, skip_serializing_if = "is_zero")]
        device: usize,
        #[serde(default, skip_serializing_if = "is_zero_group")]
        group: u32,
    },
    Read {
        offset: u64,
        len: u64,
        #[serde(default, skip_serializing_if = "is_zero")]
        device: usize,
    },
    Len {
        #[serde(default, skip_serializing_if = "is_zero")]
        device: usize,
    },
    /// How many devices the node serves. Older nodes don't know this op and drop the connection,
    /// which is how a client avoids sending a device index they would silently ignore.
    Devices,
    /// The highest fence accepted from each metadata group. Older nodes drop the connection,
    /// which is how a client avoids sending a group they would silently treat as group 0.
    Fences,
    /// Switch this connection to RDMA: the node replies `RdmaReady`, then both ends exchange
    /// queue-pair endpoints over this connection and carry on, with the same protocol, over RDMA.
    /// Needs the `rdma` feature and `ATLAS_NATIVE_RDMA=1` on the node; otherwise the node answers
    /// with an error and the connection stays as it was.
    RdmaUpgrade { gid_index: u32 },
}

fn is_zero(v: &usize) -> bool {
    *v == 0
}

fn is_zero_group(v: &u32) -> bool {
    *v == 0
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Response {
    Appended {
        offset: u64,
    },
    Written,
    Data {
        len: u64,
    },
    Len {
        len: u64,
    },
    Devices {
        count: usize,
    },
    /// `(group, fence)` pairs.
    Fences {
        fences: Vec<(u32, u64)>,
    },
    Fenced {
        current: u64,
    },
    RdmaReady,
    Error {
        message: String,
    },
}

#[derive(Debug, Default)]
struct Stats {
    appends: AtomicU64,
    writes: AtomicU64,
    reads: AtomicU64,
    fenced: AtomicU64,
    errors: AtomicU64,
    bytes_written: AtomicU64,
    bytes_read: AtomicU64,
    tls_handshake_failures: AtomicU64,
}

/// One metadata group's highest accepted fence.
struct Fence {
    current: RwLock<u64>,
    path: PathBuf,
}

struct Shared {
    devices: Vec<Arc<dyn BlockStore>>,
    root: PathBuf,
    fences: Mutex<BTreeMap<u32, Arc<Fence>>>,
    tls: Option<Arc<ServerConfig>>,
    stop: AtomicBool,
    stats: Stats,
    conns: Mutex<BTreeMap<u64, TcpStream>>,
    next_conn: AtomicU64,
    handlers: Mutex<Vec<JoinHandle<()>>>,
}

fn fence_path(root: &Path, group: u32) -> PathBuf {
    if group == 0 {
        root.join("fence")
    } else {
        root.join(format!("fence.g{group}"))
    }
}

fn read_fence(path: &Path) -> Result<u64, NativeError> {
    match fs::read_to_string(path) {
        Ok(s) => s
            .trim()
            .parse()
            .map_err(|e| NativeError::Invalid(format!("corrupt fence file: {e}"))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e.into()),
    }
}

impl Shared {
    /// `group`'s fence, read from its file the first time the group writes.
    fn fence(&self, group: u32) -> Result<Arc<Fence>, NativeError> {
        let mut fences = self
            .fences
            .lock()
            .map_err(|_| NativeError::Poisoned("fences"))?;
        if let Some(f) = fences.get(&group) {
            return Ok(f.clone());
        }
        let path = fence_path(&self.root, group);
        let f = Arc::new(Fence {
            current: RwLock::new(read_fence(&path)?),
            path,
        });
        fences.insert(group, f.clone());
        Ok(f)
    }

    fn fence_values(&self) -> BTreeMap<u32, u64> {
        self.fences
            .lock()
            .map(|f| {
                f.iter()
                    .map(|(g, f)| (*g, f.current.read().map(|c| *c).unwrap_or(0)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Runs `write` only if `fence` is not below the highest fence accepted from `group`.
    /// Writes at the current fence hold the fence lock shared, so they run concurrently;
    /// raising the fence takes it exclusively, so it waits for writes in flight and none can
    /// slip in between.
    fn fenced(
        &self,
        group: u32,
        fence: u64,
        write: impl FnOnce() -> Result<Response, NativeError>,
    ) -> Response {
        let poisoned = || Response::Error {
            message: "fence lock poisoned".into(),
        };
        let f = match self.fence(group) {
            Ok(f) => f,
            Err(e) => {
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
                return Response::Error {
                    message: e.to_string(),
                };
            }
        };
        loop {
            {
                let Ok(current) = f.current.read() else {
                    return poisoned();
                };
                if fence < *current {
                    self.stats.fenced.fetch_add(1, Ordering::Relaxed);
                    return Response::Fenced { current: *current };
                }
                if fence == *current {
                    return write().unwrap_or_else(|e| {
                        self.stats.errors.fetch_add(1, Ordering::Relaxed);
                        Response::Error {
                            message: e.to_string(),
                        }
                    });
                }
            }
            let Ok(mut current) = f.current.write() else {
                return poisoned();
            };
            if fence > *current {
                if let Err(e) = durable::write_atomic(&f.path, fence.to_string().as_bytes()) {
                    self.stats.errors.fetch_add(1, Ordering::Relaxed);
                    return Response::Error {
                        message: format!("persist fence: {e}"),
                    };
                }
                *current = fence;
            }
        }
    }

    fn device(&self, index: usize) -> Result<&Arc<dyn BlockStore>, NativeError> {
        self.devices.get(index).ok_or_else(|| {
            NativeError::Invalid(format!(
                "device {index} out of range: this node serves {}",
                self.devices.len()
            ))
        })
    }
}

pub struct DataNodeServer {
    id: String,
    shared: Arc<Shared>,
    addr: SocketAddr,
    acceptor: Option<JoinHandle<()>>,
}

impl DataNodeServer {
    /// Serves `root/nvme0.data` on an already-bound `listener` without TLS. The highest
    /// fence accepted from metadata group 0 is kept in `root/fence`, from group `g` in
    /// `root/fence.g<g>`.
    pub fn start(
        id: impl Into<String>,
        root: impl AsRef<Path>,
        listener: TcpListener,
    ) -> Result<Self, NativeError> {
        Self::start_with(id, root, listener, None)
    }

    /// Like [`Self::start`], with mutual TLS when `tls` is set.
    pub fn start_with(
        id: impl Into<String>,
        root: impl AsRef<Path>,
        listener: TcpListener,
        tls: Option<TlsIdentity>,
    ) -> Result<Self, NativeError> {
        let root = root.as_ref();
        let device: Arc<dyn BlockStore> = Arc::new(FileDevice::open(root.join("nvme0.data"))?);
        Self::start_devices(id, root, vec![device], listener, tls)
    }

    /// Serves `devices` (index = position; at least one); `root` holds the fence files.
    pub fn start_devices(
        id: impl Into<String>,
        root: impl AsRef<Path>,
        devices: Vec<Arc<dyn BlockStore>>,
        listener: TcpListener,
        tls: Option<TlsIdentity>,
    ) -> Result<Self, NativeError> {
        if devices.is_empty() {
            return Err(NativeError::Invalid(
                "a data node needs at least one device".into(),
            ));
        }
        let tls = tls.map(|t| t.server_config()).transpose()?;
        let root = root.as_ref();
        fs::create_dir_all(root)?;
        let fence = read_fence(&fence_path(root, 0))?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            devices,
            root: root.to_path_buf(),
            fences: Mutex::new(BTreeMap::from([(
                0,
                Arc::new(Fence {
                    current: RwLock::new(fence),
                    path: fence_path(root, 0),
                }),
            )])),
            tls,
            stop: AtomicBool::new(false),
            stats: Stats::default(),
            conns: Mutex::new(BTreeMap::new()),
            next_conn: AtomicU64::new(0),
            handlers: Mutex::new(Vec::new()),
        });
        let acceptor = {
            let shared = shared.clone();
            thread::spawn(move || accept_loop(listener, &shared))
        };
        Ok(Self {
            id: id.into(),
            shared,
            addr,
            acceptor: Some(acceptor),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Highest fence accepted from metadata group 0.
    pub fn fence(&self) -> u64 {
        self.group_fence(0)
    }

    /// Highest fence accepted from metadata group `group` (0 if it never wrote here).
    pub fn group_fence(&self, group: u32) -> u64 {
        self.shared.fence_values().get(&group).copied().unwrap_or(0)
    }

    /// Prometheus text exposition for this node's request counters and device size.
    pub fn render_metrics(&self) -> Result<String, NativeError> {
        let s = &self.shared.stats;
        let node = [("node", self.id.as_str())];
        let mut p = PromText::new();
        p.family(
            "atlas_native_data_requests_total",
            "counter",
            "Data-node requests served, by operation.",
        );
        for (op, v) in [
            ("append", &s.appends),
            ("write_at", &s.writes),
            ("read", &s.reads),
        ] {
            p.sample(
                "atlas_native_data_requests_total",
                &[("node", self.id.as_str()), ("op", op)],
                v.load(Ordering::Relaxed),
            );
        }
        for (name, help, v) in [
            (
                "atlas_native_data_fenced_writes_total",
                "Writes rejected for carrying a stale fence.",
                &s.fenced,
            ),
            (
                "atlas_native_data_errors_total",
                "Requests that failed on the local device.",
                &s.errors,
            ),
            (
                "atlas_native_data_written_bytes_total",
                "Bytes written to the local device.",
                &s.bytes_written,
            ),
            (
                "atlas_native_data_read_bytes_total",
                "Bytes read from the local device.",
                &s.bytes_read,
            ),
            (
                "atlas_native_data_tls_handshake_failures_total",
                "Inbound TLS handshakes that failed.",
                &s.tls_handshake_failures,
            ),
        ] {
            p.family(name, "counter", help)
                .sample(name, &node, v.load(Ordering::Relaxed));
        }
        p.family(
            "atlas_native_data_fence",
            "gauge",
            "Highest write fence (Raft term) accepted from each metadata group.",
        );
        for (group, fence) in self.shared.fence_values() {
            let group = group.to_string();
            p.sample(
                "atlas_native_data_fence",
                &[("node", self.id.as_str()), ("group", group.as_str())],
                fence,
            );
        }
        p.family(
            "atlas_native_data_device_bytes",
            "gauge",
            "Bytes of each served device in use.",
        );
        for (i, d) in self.shared.devices.iter().enumerate() {
            let index = i.to_string();
            p.sample(
                "atlas_native_data_device_bytes",
                &[("node", self.id.as_str()), ("device", index.as_str())],
                d.len()?,
            );
        }
        Ok(p.finish())
    }

    pub fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Join the acceptor first so no connection can be registered after the sweep below.
        if let Some(a) = self.acceptor.take() {
            let _ = a.join();
        }
        if let Ok(conns) = self.shared.conns.lock() {
            for c in conns.values() {
                let _ = c.shutdown(std::net::Shutdown::Both);
            }
        }
        let handlers: Vec<_> = self
            .shared
            .handlers
            .lock()
            .map(|mut h| h.drain(..).collect())
            .unwrap_or_default();
        for h in handlers {
            let _ = h.join();
        }
    }
}

impl Drop for DataNodeServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(listener: TcpListener, shared: &Arc<Shared>) {
    while !shared.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                let _ = stream.set_nodelay(true);
                let Ok(clone) = stream.try_clone() else {
                    continue;
                };
                let conn_id = shared.next_conn.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut conns) = shared.conns.lock() {
                    conns.insert(conn_id, clone);
                }
                let conn_shared = shared.clone();
                let handle = thread::spawn(move || {
                    match Conn::accept(stream, conn_shared.tls.as_ref(), HANDSHAKE_TIMEOUT) {
                        Ok(conn) => serve(conn, &conn_shared),
                        Err(_) => {
                            conn_shared
                                .stats
                                .tls_handshake_failures
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    if let Ok(mut conns) = conn_shared.conns.lock() {
                        conns.remove(&conn_id);
                    }
                });
                if let Ok(mut h) = shared.handlers.lock() {
                    h.retain(|h| !h.is_finished());
                    h.push(handle);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Whether this node may switch connections to RDMA: built with the `rdma` feature on Linux and
/// switched on by the operator with `ATLAS_NATIVE_RDMA=1`. Off by default; the data path stays
/// TCP/TLS unless asked.
fn rdma_enabled() -> bool {
    cfg!(all(feature = "rdma", target_os = "linux"))
        && std::env::var("ATLAS_NATIVE_RDMA").is_ok_and(|v| v == "1")
}

fn serve(mut stream: Conn, sh: &Shared) {
    // After a successful `RdmaUpgrade` the same request loop continues over an RDMA stream whose
    // queue-pair endpoints were exchanged over this connection. A nested upgrade is refused, and so
    // is an upgrade of a TLS connection: RDMA traffic is not encrypted, so it would silently
    // drop the protection the operator configured.
    let upgradable = matches!(stream, Conn::Plain(_));
    if let Some(_gid_index) = serve_requests(&mut stream, sh, upgradable) {
        #[cfg(all(feature = "rdma", target_os = "linux"))]
        {
            let _ = crate::rdma::with_stream(&mut stream, _gid_index, |rdma| {
                serve_requests(rdma, sh, false);
                Ok(())
            });
        }
    }
}

/// The request loop, generic over the byte stream. Returns the GID index when the client asked
/// to switch to RDMA (and the node agreed); `None` when the connection ended.
fn serve_requests<S: Read + Write>(mut stream: S, sh: &Shared, allow_upgrade: bool) -> Option<u32> {
    while let Ok(req) = read_header::<Request>(&mut stream) {
        let result = match req {
            Request::Append {
                fence,
                len,
                device,
                group,
            } => read_payload(&mut stream, len).and_then(|data| {
                let resp = sh.fenced(group, fence, || {
                    let offset = sh.device(device)?.append(fence, &data)?;
                    sh.stats.appends.fetch_add(1, Ordering::Relaxed);
                    sh.stats.bytes_written.fetch_add(len, Ordering::Relaxed);
                    Ok(Response::Appended { offset })
                });
                write_message(&mut stream, &resp, &[])
            }),
            Request::WriteAt {
                fence,
                offset,
                len,
                device,
                group,
            } => read_payload(&mut stream, len).and_then(|data| {
                let resp = sh.fenced(group, fence, || {
                    sh.device(device)?.write_at(fence, offset, &data)?;
                    sh.stats.writes.fetch_add(1, Ordering::Relaxed);
                    sh.stats.bytes_written.fetch_add(len, Ordering::Relaxed);
                    Ok(Response::Written)
                });
                write_message(&mut stream, &resp, &[])
            }),
            Request::Read {
                offset,
                len,
                device,
            } => {
                let read = if len > MAX_PAYLOAD {
                    Err(NativeError::Invalid(format!(
                        "read of {len} bytes too large"
                    )))
                } else {
                    sh.device(device)
                        .and_then(|d| d.read_exact_at(offset, len as usize))
                };
                match read {
                    Ok(buf) => {
                        sh.stats.reads.fetch_add(1, Ordering::Relaxed);
                        sh.stats.bytes_read.fetch_add(len, Ordering::Relaxed);
                        write_message(&mut stream, &Response::Data { len }, &buf)
                    }
                    Err(e) => {
                        sh.stats.errors.fetch_add(1, Ordering::Relaxed);
                        let resp = Response::Error {
                            message: e.to_string(),
                        };
                        write_message(&mut stream, &resp, &[])
                    }
                }
            }
            Request::Len { device } => {
                let resp = match sh.device(device).and_then(|d| d.len()) {
                    Ok(len) => Response::Len { len },
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                };
                write_message(&mut stream, &resp, &[])
            }
            Request::Devices => write_message(
                &mut stream,
                &Response::Devices {
                    count: sh.devices.len(),
                },
                &[],
            ),
            Request::Fences => write_message(
                &mut stream,
                &Response::Fences {
                    fences: sh.fence_values().into_iter().collect(),
                },
                &[],
            ),
            Request::RdmaUpgrade { gid_index } => {
                if allow_upgrade && rdma_enabled() {
                    if write_message(&mut stream, &Response::RdmaReady, &[]).is_err() {
                        return None;
                    }
                    return Some(gid_index);
                }
                let message = if allow_upgrade {
                    "RDMA is not enabled on this node"
                } else {
                    "RDMA is not available on this connection (already upgraded, or TLS)"
                };
                write_message(
                    &mut stream,
                    &Response::Error {
                        message: message.into(),
                    },
                    &[],
                )
            }
        };
        if result.is_err() {
            return None;
        }
    }
    None
}

/// Object-safe `Read + Write`, so [`RdmaSession`] does not need the RDMA stream's lifetimes.
#[cfg(all(feature = "rdma", target_os = "linux"))]
trait Wire: Read + Write {}
#[cfg(all(feature = "rdma", target_os = "linux"))]
impl<T: Read + Write> Wire for T {}

/// Typed requests over an RDMA connection opened by [`RemoteDevice::with_rdma`]; the same wire
/// protocol and the same fence/device/group semantics as the TCP client.
#[cfg(all(feature = "rdma", target_os = "linux"))]
pub struct RdmaSession<'a> {
    io: &'a mut dyn Wire,
    device: usize,
    group: u32,
}

#[cfg(all(feature = "rdma", target_os = "linux"))]
impl RdmaSession<'_> {
    fn expect_ok(&mut self, req: Request, payload: &[u8]) -> io::Result<(Response, Vec<u8>)> {
        write_message(&mut self.io, &req, payload)?;
        let resp: Response = read_header(&mut self.io)?;
        let data = match &resp {
            Response::Data { len } => read_payload(&mut self.io, *len)?,
            Response::Error { message } => return Err(io::Error::other(message.clone())),
            _ => Vec::new(),
        };
        Ok((resp, data))
    }

    /// Appends `data` under `fence` and returns its offset.
    pub fn append(&mut self, fence: u64, data: &[u8]) -> io::Result<u64> {
        let req = Request::Append {
            fence,
            len: data.len() as u64,
            device: self.device,
            group: self.group,
        };
        match self.expect_ok(req, data)?.0 {
            Response::Appended { offset } => Ok(offset),
            other => Err(io::Error::other(format!("append was not acknowledged: {other:?}"))),
        }
    }

    /// Reads `len` bytes at `offset`.
    pub fn read(&mut self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let req = Request::Read { offset, len, device: self.device };
        self.expect_ok(req, &[]).map(|(_, data)| data)
    }
}

/// Idle connections a [`RemoteDevice`] keeps open for reuse.
const MAX_IDLE_CONNS: usize = 8;

/// Client for a [`DataNodeServer`]. Concurrent requests each use their own connection (one is
/// opened when none is idle); up to [`MAX_IDLE_CONNS`] are kept open between requests.
pub struct RemoteDevice {
    target: String,
    timeout: Duration,
    tls: Option<(Arc<ClientConfig>, ServerName<'static>)>,
    idle: Mutex<Vec<Conn>>,
    /// Which of the node's devices this client addresses.
    device: usize,
    /// For `device > 0`: the node has confirmed it serves that many devices.
    device_checked: AtomicBool,
    /// The metadata group whose fence this client's writes carry.
    group: u32,
    /// For `group > 0`: the node has confirmed it keeps a fence per group.
    group_checked: AtomicBool,
}

impl std::fmt::Debug for RemoteDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteDevice")
            .field("target", &self.target)
            .field("device", &self.device)
            .field("group", &self.group)
            .field("tls", &self.tls.as_ref().map(|(_, n)| n))
            .finish()
    }
}

impl RemoteDevice {
    /// Plaintext client for `target`: a socket address or a `host:port` re-resolved on every
    /// connection. `timeout` bounds connecting, the handshake and each socket read or write.
    pub fn new(target: impl std::fmt::Display, timeout: Duration) -> Self {
        Self {
            target: target.to_string(),
            timeout,
            tls: None,
            idle: Mutex::new(Vec::new()),
            device: 0,
            device_checked: AtomicBool::new(false),
            group: 0,
            group_checked: AtomicBool::new(false),
        }
    }

    /// Mutual-TLS client that requires the data node's certificate to be valid for `node_id`.
    pub fn with_tls(
        target: impl std::fmt::Display,
        node_id: &str,
        identity: &TlsIdentity,
        timeout: Duration,
    ) -> Result<Self, NativeError> {
        Ok(Self {
            target: target.to_string(),
            timeout,
            tls: Some((identity.client_config()?, tls::server_name(node_id)?)),
            idle: Mutex::new(Vec::new()),
            device: 0,
            device_checked: AtomicBool::new(false),
            group: 0,
            group_checked: AtomicBool::new(false),
        })
    }

    /// Addresses the node's device `index` instead of device 0.
    pub fn on_device(mut self, index: usize) -> Self {
        self.device = index;
        self
    }

    /// Fences this client's writes as metadata group `group` instead of group 0.
    pub fn in_group(mut self, group: u32) -> Self {
        self.group = group;
        self
    }

    /// Before the first write for a group other than 0, confirms the node keeps a fence per
    /// group: one that predates groups would fence every group's writes against one term.
    fn check_group(&self) -> Result<(), NativeError> {
        if self.group == 0 || self.group_checked.load(Ordering::Acquire) {
            return Ok(());
        }
        match self.exchange_pooled(&Request::Fences, &[]) {
            Ok((Response::Fences { .. }, _)) => {
                self.group_checked.store(true, Ordering::Release);
                Ok(())
            }
            Ok((other, _)) => Err(unexpected(other)),
            Err(e) => Err(NativeError::Remote(format!(
                "data node {} does not keep per-group fences (older version?): {e}",
                self.target
            ))),
        }
    }

    /// Before the first request to a device other than 0, confirms the node serves it: a node
    /// that predates multiple devices would otherwise ignore the index and use device 0.
    fn check_device(&self) -> Result<(), NativeError> {
        if self.device == 0 || self.device_checked.load(Ordering::Acquire) {
            return Ok(());
        }
        match self.exchange_pooled(&Request::Devices, &[]) {
            Ok((Response::Devices { count }, _)) if count > self.device => {
                self.device_checked.store(true, Ordering::Release);
                Ok(())
            }
            Ok((Response::Devices { count }, _)) => Err(NativeError::Invalid(format!(
                "data node {} serves {count} devices; device {} is not one of them",
                self.target, self.device
            ))),
            Ok((other, _)) => Err(unexpected(other)),
            Err(e) => Err(NativeError::Remote(format!(
                "data node {} did not report its devices (older version?): {e}",
                self.target
            ))),
        }
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    fn connect(&self) -> io::Result<Conn> {
        let s = TcpStream::connect_timeout(&tls::resolve(&self.target)?, self.timeout)?;
        s.set_nodelay(true)?;
        s.set_read_timeout(Some(self.timeout))?;
        s.set_write_timeout(Some(self.timeout))?;
        Conn::connect(s, self.tls.as_ref().map(|(c, n)| (c, n.clone())))
    }

    /// Opens a fresh connection, asks the node to switch it to RDMA and runs `f` over the queue
    /// pair. A node that has RDMA off, or a TLS client (RDMA traffic is not encrypted), yields
    /// `ErrorKind::Unsupported` so the caller can use the ordinary TCP methods instead. The
    /// connection is not pooled: the RDMA objects live only inside this call.
    #[cfg(all(feature = "rdma", target_os = "linux"))]
    pub fn with_rdma<R>(
        &self,
        gid_index: u32,
        f: impl FnOnce(&mut RdmaSession<'_>) -> io::Result<R>,
    ) -> io::Result<R> {
        if self.tls.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "RDMA traffic is not encrypted; refused on a TLS client",
            ));
        }
        let mut conn = self.connect()?;
        write_message(&mut conn, &Request::RdmaUpgrade { gid_index }, &[])?;
        match read_header::<Response>(&mut conn)? {
            Response::RdmaReady => {}
            Response::Error { message } => {
                return Err(io::Error::new(io::ErrorKind::Unsupported, message))
            }
            other => {
                return Err(io::Error::other(format!("unexpected upgrade reply: {other:?}")))
            }
        }
        let (device, group) = (self.device, self.group);
        crate::rdma::with_stream(&mut conn, gid_index, |stream| {
            f(&mut RdmaSession { io: stream, device, group })
        })
    }

    fn exchange(s: &mut Conn, req: &Request, payload: &[u8]) -> io::Result<(Response, Vec<u8>)> {
        write_message(s, req, payload)?;
        let resp: Response = read_header(s)?;
        let data = match &resp {
            Response::Data { len } => read_payload(s, *len)?,
            _ => Vec::new(),
        };
        Ok((resp, data))
    }

    /// Sends one request. A failure on a reused connection (e.g. the node restarted) is retried
    /// once on a fresh one; a retried append can therefore leave an unreferenced copy behind.
    fn call(&self, req: &Request, payload: &[u8]) -> Result<(Response, Vec<u8>), NativeError> {
        self.check_device()?;
        let out = self.exchange_pooled(req, payload)?;
        match out.0 {
            Response::Fenced { current } => Err(NativeError::Fenced { current }),
            Response::Error { message } => Err(NativeError::Remote(message)),
            _ => Ok(out),
        }
    }

    fn exchange_pooled(
        &self,
        req: &Request,
        payload: &[u8],
    ) -> Result<(Response, Vec<u8>), NativeError> {
        let pooled = self
            .idle
            .lock()
            .map_err(|_| NativeError::Poisoned("remote device"))?
            .pop();
        let reused = pooled.is_some();
        let mut stream = match pooled {
            Some(s) => s,
            None => self.connect()?,
        };
        let out = match Self::exchange(&mut stream, req, payload) {
            Err(_) if reused => {
                stream = self.connect()?;
                Self::exchange(&mut stream, req, payload)
            }
            other => other,
        }?;
        if let Ok(mut idle) = self.idle.lock() {
            if idle.len() < MAX_IDLE_CONNS {
                idle.push(stream);
            }
        }
        Ok(out)
    }
}

fn unexpected(resp: Response) -> NativeError {
    NativeError::Remote(format!("unexpected data-node response: {resp:?}"))
}

impl BlockStore for RemoteDevice {
    fn append(&self, fence: u64, data: &[u8]) -> Result<u64, NativeError> {
        self.check_group()?;
        let req = Request::Append {
            fence,
            len: data.len() as u64,
            device: self.device,
            group: self.group,
        };
        match self.call(&req, data)?.0 {
            Response::Appended { offset } => Ok(offset),
            other => Err(unexpected(other)),
        }
    }

    fn write_at(&self, fence: u64, offset: u64, data: &[u8]) -> Result<(), NativeError> {
        self.check_group()?;
        let req = Request::WriteAt {
            fence,
            offset,
            len: data.len() as u64,
            device: self.device,
            group: self.group,
        };
        match self.call(&req, data)?.0 {
            Response::Written => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    fn read_exact_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, NativeError> {
        let req = Request::Read {
            offset,
            len: len as u64,
            device: self.device,
        };
        match self.call(&req, &[])? {
            (Response::Data { .. }, data) => Ok(data),
            (other, _) => Err(unexpected(other)),
        }
    }

    fn len(&self) -> Result<u64, NativeError> {
        match self
            .call(
                &Request::Len {
                    device: self.device,
                },
                &[],
            )?
            .0
        {
            Response::Len { len } => Ok(len),
            other => Err(unexpected(other)),
        }
    }

    fn endpoint(&self) -> Option<&str> {
        Some(&self.target)
    }
}

fn write_message(w: &mut impl Write, header: &impl Serialize, payload: &[u8]) -> io::Result<()> {
    let body = serde_json::to_vec(header).map_err(io::Error::other)?;
    if body.len() > MAX_HEADER || payload.len() as u64 > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "data-node message too large",
        ));
    }
    // The header goes out in one write and a large payload in a second, rather than copying
    // the payload into one frame buffer first.
    let mut head = Vec::with_capacity(4 + body.len() + payload.len().min(SMALL_PAYLOAD));
    head.extend_from_slice(&(body.len() as u32).to_be_bytes());
    head.extend_from_slice(&body);
    if payload.len() <= SMALL_PAYLOAD {
        head.extend_from_slice(payload);
        w.write_all(&head)?;
    } else {
        w.write_all(&head)?;
        w.write_all(payload)?;
    }
    w.flush()
}

fn read_header<T: for<'de> Deserialize<'de>>(r: &mut impl Read) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "data-node header too large",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn read_payload(r: &mut impl Read, len: u64) -> io::Result<Vec<u8>> {
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "data-node payload too large",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trip_and_size_guards() {
        let mut buf = Vec::new();
        write_message(
            &mut buf,
            &Request::WriteAt {
                device: 2,
                fence: 7,
                offset: 9,
                len: 3,
                group: 4,
            },
            b"abc",
        )
        .unwrap();
        let mut r = buf.as_slice();
        match read_header::<Request>(&mut r).unwrap() {
            Request::WriteAt {
                device,
                fence,
                offset,
                len,
                group,
            } => {
                assert_eq!((device, fence, offset, len, group), (2, 7, 9, 3, 4));
                assert_eq!(read_payload(&mut r, len).unwrap(), b"abc");
            }
            other => panic!("unexpected {other:?}"),
        }

        // Device 0 and group 0 stay off the wire, so older data nodes parse requests unchanged.
        let legacy = serde_json::to_string(&Request::Len { device: 0 }).unwrap();
        assert!(!legacy.contains("device"), "{legacy}");
        let legacy = serde_json::to_string(&Request::Append {
            fence: 1,
            len: 1,
            device: 0,
            group: 0,
        })
        .unwrap();
        assert!(!legacy.contains("group"), "{legacy}");

        let mut huge = ((MAX_HEADER + 1) as u32).to_be_bytes().to_vec();
        huge.extend_from_slice(b"{}");
        assert!(read_header::<Request>(&mut huge.as_slice()).is_err());
        assert!(read_payload(&mut [].as_slice(), MAX_PAYLOAD + 1).is_err());
    }
    fn start_node() -> (tempfile::TempDir, DataNodeServer) {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let node = DataNodeServer::start("n1", dir.path(), listener).unwrap();
        (dir, node)
    }

    /// A client that asks for RDMA on a node where it is off (the default, and always the case
    /// without the `rdma` feature) gets a clean error, and the connection keeps working over TCP.
    #[test]
    fn an_rdma_upgrade_is_refused_cleanly_when_not_enabled() {
        if rdma_enabled() {
            return; // the node would agree; covered by the ignored RDMA test
        }
        let (_dir, node) = start_node();
        let mut tcp = TcpStream::connect(node.local_addr()).unwrap();
        write_message(&mut tcp, &Request::RdmaUpgrade { gid_index: 1 }, &[]).unwrap();
        match read_header::<Response>(&mut tcp).unwrap() {
            Response::Error { message } => assert!(message.contains("not enabled"), "{message}"),
            other => panic!("expected an error, got {other:?}"),
        }
        write_message(&mut tcp, &Request::Devices, &[]).unwrap();
        assert!(matches!(
            read_header::<Response>(&mut tcp).unwrap(),
            Response::Devices { count: 1 }
        ));
    }

    /// The node serves the same protocol over RDMA after an upgrade: append and read back through
    /// a queue pair on the host's RDMA device (SoftRoCE is enough). `cargo test -p atlas-native
    /// --features rdma --lib -- --ignored rdma`.
    #[cfg(all(feature = "rdma", target_os = "linux"))]
    #[test]
    #[ignore = "needs an RDMA device (e.g. SoftRoCE rxe0)"]
    fn node_serves_requests_over_rdma_after_an_upgrade() {
        std::env::set_var("ATLAS_NATIVE_RDMA", "1");
        let gid: u32 = std::env::var("ATLAS_RDMA_GID_INDEX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let (_dir, node) = start_node();
        let mut tcp = TcpStream::connect(node.local_addr()).unwrap();
        write_message(&mut tcp, &Request::RdmaUpgrade { gid_index: gid }, &[]).unwrap();
        assert!(matches!(
            read_header::<Response>(&mut tcp).unwrap(),
            Response::RdmaReady
        ));
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 249) as u8).collect();
        let back = crate::rdma::with_stream(&mut tcp, gid, |s| {
            write_message(
                s,
                &Request::Append { fence: 1, len: payload.len() as u64, device: 0, group: 0 },
                &payload,
            )?;
            let Response::Appended { offset } = read_header::<Response>(s)? else {
                return Err(io::Error::other("append was not acknowledged"));
            };
            write_message(
                s,
                &Request::Read { offset, len: payload.len() as u64, device: 0 },
                &[],
            )?;
            let Response::Data { len } = read_header::<Response>(s)? else {
                return Err(io::Error::other("read returned no data"));
            };
            read_payload(s, len)
        })
        .expect("requests over RDMA");
        assert_eq!(back, payload);
    }

    /// The client helper: `RemoteDevice::with_rdma` upgrades, appends and reads back through a
    /// typed session. Same SoftRoCE prerequisites as the test above.
    #[cfg(all(feature = "rdma", target_os = "linux"))]
    #[test]
    #[ignore = "needs an RDMA device (e.g. SoftRoCE rxe0)"]
    fn remote_device_with_rdma_appends_and_reads_back() {
        std::env::set_var("ATLAS_NATIVE_RDMA", "1");
        let gid: u32 = std::env::var("ATLAS_RDMA_GID_INDEX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let (_dir, node) = start_node();
        let client = RemoteDevice::new(node.local_addr(), Duration::from_secs(10));
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 241) as u8).collect();
        let back = client
            .with_rdma(gid, |s| {
                let offset = s.append(1, &payload)?;
                s.read(offset, payload.len() as u64)
            })
            .expect("RDMA session");
        assert_eq!(back, payload);
    }

    /// Without the operator switch the helper reports `Unsupported`, which is the caller's cue to
    /// use the TCP methods.
    #[cfg(all(feature = "rdma", target_os = "linux"))]
    #[test]
    fn with_rdma_reports_unsupported_when_the_node_has_it_off() {
        if rdma_enabled() {
            return;
        }
        let (_dir, node) = start_node();
        let client = RemoteDevice::new(node.local_addr(), Duration::from_secs(5));
        let err = client.with_rdma(1, |_| Ok(())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported, "{err}");
    }
}
