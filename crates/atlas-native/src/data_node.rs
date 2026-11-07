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
    time::{Duration, Instant},
};

use rustls::{pki_types::ServerName, ClientConfig, ServerConfig};
use serde::{Deserialize, Serialize};

use crate::{
    device::{BlockStore, FileDevice},
    durable,
    engine::NativeError,
    grant::{Grant, GrantKey},
    metrics::PromText,
    rdma_link::{
        is_enomem, ConnSlot, Duplex, RdmaConfig, RdmaSnapshot, RdmaTransport,
        ENOMEM_HINT, NODE_REFUSED_RETRY_AFTER,
    },
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
        /// Write authorization; required by a node that holds a [`GrantKey`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grant: Option<Grant>,
    },
    WriteAt {
        fence: u64,
        offset: u64,
        len: u64,
        #[serde(default, skip_serializing_if = "is_zero")]
        device: usize,
        #[serde(default, skip_serializing_if = "is_zero_group")]
        group: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grant: Option<Grant>,
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
        /// With a grant: proof the node placed the data at `offset` ([`GrantKey::receipt`]), so
        /// the leader can trust a client-reported append offset.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receipt: Option<String>,
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
    /// RDMA upgrades whose queue-pair setup failed on this node (the client then falls back).
    rdma_setup_failures: AtomicU64,
    /// The subset of those that failed with `ENOMEM`, i.e. the node's `RLIMIT_MEMLOCK`.
    rdma_enomem: AtomicU64,
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
    /// This node's id, which grants are bound to.
    id: String,
    /// When set, every write must carry a [`Grant`] made with this key.
    grant_key: Option<GrantKey>,
    /// Transport offered for `RdmaUpgrade`; unset means the verbs transport when the operator
    /// enabled it (`ATLAS_NATIVE_RDMA=1`), else none.
    rdma: RwLock<Option<Arc<dyn RdmaTransport>>>,
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
    /// The transport this node would switch a plaintext connection to, if it offers RDMA.
    fn rdma_offer(&self) -> Option<Arc<dyn RdmaTransport>> {
        if let Some(t) = self.rdma.read().ok().and_then(|g| g.clone()) {
            return Some(t);
        }
        if rdma_enabled() {
            return crate::rdma_link::verbs_transport();
        }
        None
    }

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

    /// Whether a write may proceed: always, unless this node holds a grant key, in which case
    /// `grant` must authorize exactly this write.
    fn authorize(
        &self,
        grant: Option<&Grant>,
        fence: u64,
        group: u32,
        device: usize,
        offset: Option<u64>,
        len: u64,
    ) -> Result<(), Response> {
        let Some(key) = &self.grant_key else {
            return Ok(());
        };
        let refused = |message: String| {
            self.stats.errors.fetch_add(1, Ordering::Relaxed);
            Response::Error { message }
        };
        let grant = grant.ok_or_else(|| refused("write grant required".into()))?;
        key.authorize(grant, &self.id, fence, group, device, offset, len)
            .map_err(refused)
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
        Self::start_devices_with(id, root, devices, listener, tls, None)
    }

    /// [`Self::start_devices`] that, with a `grant_key`, refuses every write not authorized by a
    /// [`Grant`] made with it (see [`crate::grant`]).
    pub fn start_devices_with(
        id: impl Into<String>,
        root: impl AsRef<Path>,
        devices: Vec<Arc<dyn BlockStore>>,
        listener: TcpListener,
        tls: Option<TlsIdentity>,
        grant_key: Option<GrantKey>,
    ) -> Result<Self, NativeError> {
        let id = id.into();
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
            id: id.clone(),
            grant_key,
            rdma: RwLock::new(None),
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
            id,
            shared,
            addr,
            acceptor: Some(acceptor),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Offers `transport` for `RdmaUpgrade` requests on plaintext connections, regardless of
    /// `ATLAS_NATIVE_RDMA` (which only switches on the verbs transport). Used to test the
    /// RDMA client path with [`crate::rdma_link::FakeTransport`] on hosts without RDMA.
    pub fn with_rdma_transport(self, transport: Arc<dyn RdmaTransport>) -> Self {
        self.set_rdma_transport(transport);
        self
    }

    /// [`Self::with_rdma_transport`] on a running server; connections already upgraded keep
    /// theirs.
    pub fn set_rdma_transport(&self, transport: Arc<dyn RdmaTransport>) {
        if let Ok(mut g) = self.shared.rdma.write() {
            *g = Some(transport);
        }
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
            (
                "atlas_native_data_rdma_setup_failures_total",
                "RDMA upgrades whose queue-pair setup failed on this node.",
                &s.rdma_setup_failures,
            ),
            (
                "atlas_native_data_rdma_enomem_total",
                "RDMA setups that failed with ENOMEM (raise the node's RLIMIT_MEMLOCK).",
                &s.rdma_enomem,
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
    // setup was exchanged over this connection. A nested upgrade is refused, and so is an
    // upgrade of a TLS connection: RDMA traffic is not encrypted, so it would silently drop the
    // protection the operator configured.
    let upgradable = matches!(stream, Conn::Plain(_));
    if let Some(gid_index) = serve_requests(&mut stream, sh, upgradable) {
        if let (Conn::Plain(tcp), Some(transport)) = (&mut stream, sh.rdma_offer()) {
            // Only a setup failure returns an error (the request loop's own end is not one); it
            // is counted so an operator can see a node that cannot pin memory (`ENOMEM`), and
            // the client sees the connection reset and falls back to TCP.
            if let Err(e) = transport.serve(tcp, gid_index, &mut |rdma| {
                serve_requests(&mut IdleRdma::new(rdma, &sh.stop), sh, false);
            }) {
                sh.stats.rdma_setup_failures.fetch_add(1, Ordering::Relaxed);
                if is_enomem(&e) {
                    sh.stats.rdma_enomem.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// How long a started message may stall on an RDMA stream before the connection is given up.
const RDMA_MESSAGE_STALL: Duration = Duration::from_secs(30);

/// Serves an RDMA stream whose reads wake up regularly (`ErrorKind::TimedOut`): a quiet
/// connection between two requests is healthy and is waited on until the node stops, instead of
/// being closed after the stream's read timeout. A message that has begun but stalls for
/// [`RDMA_MESSAGE_STALL`] is still an error.
struct IdleRdma<'a, S: ?Sized> {
    inner: &'a mut S,
    stop: &'a AtomicBool,
    /// True from the start of the connection and after each write (a response ends a request),
    /// until the next byte arrives: the stream is waiting for a request, not in the middle of one.
    between_requests: bool,
    last_progress: Instant,
}

impl<'a, S: ?Sized> IdleRdma<'a, S> {
    fn new(inner: &'a mut S, stop: &'a AtomicBool) -> Self {
        Self {
            inner,
            stop,
            between_requests: true,
            last_progress: Instant::now(),
        }
    }
}

impl<S: Read + ?Sized> Read for IdleRdma<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                    if self.stop.load(Ordering::SeqCst) {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "data node is shutting down",
                        ));
                    }
                    if !self.between_requests && self.last_progress.elapsed() > RDMA_MESSAGE_STALL
                    {
                        return Err(e);
                    }
                }
                Ok(n) => {
                    if n > 0 {
                        self.between_requests = false;
                        self.last_progress = Instant::now();
                    }
                    return Ok(n);
                }
                other => return other,
            }
        }
    }
}

impl<S: Write + ?Sized> Write for IdleRdma<'_, S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.between_requests = true;
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
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
                grant,
            } => read_payload(&mut stream, len).and_then(|data| {
                if let Err(refused) = sh.authorize(grant.as_ref(), fence, group, device, None, len)
                {
                    return write_message(&mut stream, &refused, &[]);
                }
                let resp = sh.fenced(group, fence, || {
                    let offset = sh.device(device)?.append(fence, &data)?;
                    sh.stats.appends.fetch_add(1, Ordering::Relaxed);
                    sh.stats.bytes_written.fetch_add(len, Ordering::Relaxed);
                    let receipt = match (&sh.grant_key, &grant) {
                        (Some(k), Some(g)) => Some(k.receipt(&g.id, offset)),
                        _ => None,
                    };
                    Ok(Response::Appended { offset, receipt })
                });
                write_message(&mut stream, &resp, &[])
            }),
            Request::WriteAt {
                fence,
                offset,
                len,
                device,
                group,
                grant,
            } => read_payload(&mut stream, len).and_then(|data| {
                if let Err(refused) =
                    sh.authorize(grant.as_ref(), fence, group, device, Some(offset), len)
                {
                    return write_message(&mut stream, &refused, &[]);
                }
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
                if allow_upgrade && sh.rdma_offer().is_some() {
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

/// A pooled connection: ordinary TCP/TLS, or an RDMA stream whose verbs resources are owned by a
/// worker thread (see [`crate::rdma_link`]).
enum Link {
    Net(Conn),
    /// The stream and its slot in the shared live-connection count.
    Rdma(Box<dyn Duplex>, #[allow(dead_code)] ConnSlot),
}

impl Link {
    fn is_rdma(&self) -> bool {
        matches!(self, Link::Rdma(..))
    }
}

impl Read for Link {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Link::Net(c) => c.read(buf),
            Link::Rdma(r, _) => r.read(buf),
        }
    }
}

impl Write for Link {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Link::Net(c) => c.write(buf),
            Link::Rdma(r, _) => r.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Link::Net(c) => c.flush(),
            Link::Rdma(r, _) => r.flush(),
        }
    }
}

/// A device's RDMA settings plus its back-off state.
struct RdmaClient {
    cfg: RdmaConfig,
    /// RDMA is not attempted before this instant (after a failed attempt or connection).
    retry_at: Mutex<Option<Instant>>,
}

impl RdmaClient {
    fn usable(&self) -> bool {
        match self.retry_at.lock() {
            Ok(g) => g.is_none_or(|t| Instant::now() >= t),
            Err(_) => false,
        }
    }

    fn back_off(&self, d: Duration) {
        if let Ok(mut g) = self.retry_at.lock() {
            *g = Some(Instant::now() + d);
        }
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
    idle: Mutex<Vec<Link>>,
    /// Opt-in RDMA transport for this node's connections, with TCP as the fallback.
    rdma: Option<RdmaClient>,
    /// Which of the node's devices this client addresses.
    device: usize,
    /// For `device > 0`: the node has confirmed it serves that many devices.
    device_checked: AtomicBool,
    /// The metadata group whose fence this client's writes carry.
    group: u32,
    /// For `group > 0`: the node has confirmed it keeps a fence per group.
    group_checked: AtomicBool,
    /// Signs this client's own writes for nodes that require write grants (a metadata leader
    /// holds it; a direct-write client never does and passes the grants it was leased).
    grant_key: Option<GrantKey>,
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
            rdma: None,
            device: 0,
            device_checked: AtomicBool::new(false),
            group: 0,
            group_checked: AtomicBool::new(false),
            grant_key: None,
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
            rdma: None,
            device: 0,
            device_checked: AtomicBool::new(false),
            group: 0,
            group_checked: AtomicBool::new(false),
            grant_key: None,
        })
    }

    /// Makes this client authorize its own writes with `key`, for data nodes that require write
    /// grants ([`crate::grant`]).
    pub fn with_grant_key(mut self, key: GrantKey) -> Self {
        self.grant_key = Some(key);
        self
    }

    /// Prefers RDMA for this node's connections: each new connection asks the node to switch to
    /// RDMA, and pooled RDMA connections are reused like TCP ones. Any failure (the node does not
    /// offer it, the transport cannot connect, a request errors mid-flight) falls back to
    /// ordinary TCP, is counted in `cfg.stats`, and pauses RDMA attempts on this device for
    /// `cfg.retry_after` (five minutes when the node itself declined).
    ///
    /// RDMA carries the protocol in plaintext, so this is a configuration error on a client that
    /// uses TLS: the operator asked for encryption and must not silently lose it.
    pub fn use_rdma(mut self, cfg: RdmaConfig) -> Result<Self, NativeError> {
        if self.tls.is_some() {
            return Err(NativeError::Invalid(format!(
                "RDMA is plaintext and cannot be combined with TLS (data node {}); \
                 use TLS over TCP or RDMA on a trusted storage network, not both",
                self.target
            )));
        }
        cfg.warn_if_memlock_low();
        self.rdma = Some(RdmaClient {
            cfg,
            retry_at: Mutex::new(None),
        });
        Ok(self)
    }

    /// Closes the idle pooled connections; the next request opens a new one (and so tries RDMA
    /// again if its back-off has passed).
    pub fn close_idle(&self) {
        if let Ok(mut idle) = self.idle.lock() {
            idle.clear();
        }
    }

    /// RDMA counters when [`Self::use_rdma`] was set.
    pub fn rdma_stats(&self) -> Option<RdmaSnapshot> {
        self.rdma.as_ref().map(|r| r.cfg.stats.snapshot())
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

    /// Writes `data` at `offset` carrying `grant`, the permission a leader leased to this client
    /// (see [`crate::grant`]). Fails with [`NativeError::Fenced`] once the node has accepted a
    /// write from a later term than the grant's.
    pub fn write_at_granted(
        &self,
        grant: Option<Grant>,
        fence: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<(), NativeError> {
        self.check_group()?;
        let req = Request::WriteAt {
            fence,
            offset,
            len: data.len() as u64,
            device: self.device,
            group: self.group,
            grant,
        };
        match self.call(&req, data)?.0 {
            Response::Written => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    /// Appends `data` carrying `grant`; returns the offset and, from a node that checks grants,
    /// its receipt for that offset.
    pub fn append_granted(
        &self,
        grant: Option<Grant>,
        fence: u64,
        data: &[u8],
    ) -> Result<(u64, Option<String>), NativeError> {
        self.check_group()?;
        let req = Request::Append {
            fence,
            len: data.len() as u64,
            device: self.device,
            group: self.group,
            grant,
        };
        match self.call(&req, data)?.0 {
            Response::Appended { offset, receipt } => Ok((offset, receipt)),
            other => Err(unexpected(other)),
        }
    }

    fn connect(&self) -> io::Result<Link> {
        if let Some(r) = &self.rdma {
            if r.usable() {
                // Each RDMA connection pins memory on both ends, so only so many may be alive
                // at once; the rest use TCP, which is not a failure and does not back RDMA off.
                let Some(slot) = r.cfg.stats.reserve(r.cfg.max_connections) else {
                    r.cfg.stats.capped_fallback();
                    return self.connect_net().map(Link::Net);
                };
                match self.connect_rdma(r, slot) {
                    Ok(link) => {
                        r.cfg.stats.connected();
                        return Ok(link);
                    }
                    Err(e) => {
                        r.cfg.stats.connect_failed();
                        if is_enomem(&e) {
                            r.cfg.stats.enomem();
                            r.cfg.note(ENOMEM_HINT);
                        }
                        r.back_off(if e.kind() == io::ErrorKind::Unsupported {
                            NODE_REFUSED_RETRY_AFTER
                        } else {
                            r.cfg.retry_after
                        });
                    }
                }
            }
            r.cfg.stats.tcp_fallback();
        }
        self.connect_net().map(Link::Net)
    }

    fn connect_tcp(&self) -> io::Result<TcpStream> {
        let s = TcpStream::connect_timeout(&tls::resolve(&self.target)?, self.timeout)?;
        s.set_nodelay(true)?;
        s.set_read_timeout(Some(self.timeout))?;
        s.set_write_timeout(Some(self.timeout))?;
        Ok(s)
    }

    fn connect_net(&self) -> io::Result<Conn> {
        Conn::connect(self.connect_tcp()?, self.tls.as_ref().map(|(c, n)| (c, n.clone())))
    }

    /// Opens a connection, asks the node to switch it to RDMA and hands it to the transport. A
    /// node that does not offer RDMA yields `ErrorKind::Unsupported`.
    fn connect_rdma(&self, r: &RdmaClient, slot: ConnSlot) -> io::Result<Link> {
        let mut tcp = self.connect_tcp()?;
        let gid_index = r.cfg.gid_index;
        write_message(&mut tcp, &Request::RdmaUpgrade { gid_index }, &[])?;
        match read_header::<Response>(&mut tcp)? {
            Response::RdmaReady => {}
            Response::Error { message } => {
                return Err(io::Error::new(io::ErrorKind::Unsupported, message))
            }
            other => {
                return Err(io::Error::other(format!("unexpected upgrade reply: {other:?}")))
            }
        }
        r.cfg.transport.connect(tcp, gid_index).map(|d| Link::Rdma(d, slot))
    }

    fn exchange(s: &mut Link, req: &Request, payload: &[u8]) -> io::Result<(Response, Vec<u8>)> {
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
        let was_rdma = stream.is_rdma();
        let out = match Self::exchange(&mut stream, req, payload) {
            Err(_) if reused || was_rdma => {
                // A broken RDMA connection also pauses RDMA, so the retry goes over TCP.
                if let (true, Some(r)) = (was_rdma, &self.rdma) {
                    r.cfg.stats.midflight_error();
                    r.back_off(r.cfg.retry_after);
                }
                stream = self.connect()?;
                Self::exchange(&mut stream, req, payload)
            }
            other => other,
        }?;
        if stream.is_rdma() {
            if let Some(r) = &self.rdma {
                r.cfg.stats.request();
            }
        }
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
        let grant = self.grant_key.as_ref().map(|k| {
            k.mint_own(fence, self.group, self.device, None, data.len() as u64)
        });
        self.append_granted(grant, fence, data).map(|(offset, _)| offset)
    }

    fn write_at(&self, fence: u64, offset: u64, data: &[u8]) -> Result<(), NativeError> {
        let grant = self.grant_key.as_ref().map(|k| {
            k.mint_own(fence, self.group, self.device, Some(offset), data.len() as u64)
        });
        self.write_at_granted(grant, fence, offset, data)
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
                grant: None,
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
                ..
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
            grant: None,
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
                &Request::Append { fence: 1, len: payload.len() as u64, device: 0, group: 0, grant: None },
                &payload,
            )?;
            let Response::Appended { offset, .. } = read_header::<Response>(s)? else {
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

    /// The pooled client: `RemoteDevice::use_rdma` with the verbs transport appends and reads back
    /// over a long-lived queue pair owned by a worker thread, reusing it across requests. Same
    /// SoftRoCE prerequisites as the test above.
    #[cfg(all(feature = "rdma", target_os = "linux"))]
    #[test]
    #[ignore = "needs an RDMA device (e.g. SoftRoCE rxe0)"]
    fn pooled_remote_device_uses_rdma_and_reuses_the_connection() {
        std::env::set_var("ATLAS_NATIVE_RDMA", "1");
        let gid: u32 = std::env::var("ATLAS_RDMA_GID_INDEX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let (_dir, node) = start_node();
        let cfg = RdmaConfig::new(crate::rdma_link::verbs_transport().unwrap(), gid);
        let client = RemoteDevice::new(node.local_addr(), Duration::from_secs(10))
            .use_rdma(cfg)
            .unwrap();
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 241) as u8).collect();
        for _ in 0..3 {
            let offset = client.append(1, &payload).unwrap();
            assert_eq!(client.read_exact_at(offset, payload.len()).unwrap(), payload);
        }
        let s = client.rdma_stats().unwrap();
        assert_eq!((s.connects, s.requests), (1, 6), "{s:?}");
        assert_eq!((s.connect_failures, s.tcp_fallback_connects), (0, 0), "{s:?}");
    }

    /// An idle pooled RDMA connection must survive longer than the stream's 30 s read timeout: the
    /// node must not end a healthy connection just because no request arrived. Takes ~40 s.
    #[cfg(all(feature = "rdma", target_os = "linux"))]
    #[test]
    #[ignore = "needs an RDMA device (e.g. SoftRoCE rxe0); takes ~40 s"]
    fn an_idle_pooled_rdma_connection_survives_the_stream_timeout() {
        std::env::set_var("ATLAS_NATIVE_RDMA", "1");
        let (_dir, node) = start_node();
        let client = rdma_client(&node);
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 239) as u8).collect();
        let offset = client.append(1, &payload).unwrap();
        thread::sleep(Duration::from_secs(40));
        let t = Instant::now();
        let back = client.read_exact_at(offset, payload.len());
        let took = t.elapsed();
        let s = client.rdma_stats().unwrap();
        let ok = back.as_ref().is_ok_and(|b| *b == payload);
        assert!(
            ok && s.midflight_errors == 0 && s.connects == 1 && s.requests == 2,
            "read ok {ok}, {s:?}, request after idle took {took:?}"
        );
    }

    /// A node shuts down promptly while a client still holds a pooled RDMA connection open: its
    /// handler thread must not sit in a 30 s RDMA read.
    #[cfg(all(feature = "rdma", target_os = "linux"))]
    #[test]
    #[ignore = "needs an RDMA device (e.g. SoftRoCE rxe0)"]
    fn shutdown_does_not_wait_for_an_idle_pooled_rdma_connection() {
        std::env::set_var("ATLAS_NATIVE_RDMA", "1");
        let (_dir, mut node) = start_node();
        let client = rdma_client(&node);
        client.append(1, b"hello").unwrap();
        let t = Instant::now();
        node.shutdown();
        let took = t.elapsed();
        let s = client.rdma_stats().unwrap();
        assert!(
            s.requests == 1 && took < Duration::from_secs(5),
            "{s:?}, shutdown took {took:?}"
        );
    }

    #[cfg(all(feature = "rdma", target_os = "linux"))]
    fn rdma_client(node: &DataNodeServer) -> RemoteDevice {
        let gid: u32 = std::env::var("ATLAS_RDMA_GID_INDEX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let cfg = RdmaConfig::new(crate::rdma_link::verbs_transport().unwrap(), gid);
        RemoteDevice::new(node.local_addr(), Duration::from_secs(60))
            .use_rdma(cfg)
            .unwrap()
    }

    /// A reader that times out `timeouts` times before yielding `data`.
    struct Quiet {
        timeouts: usize,
        data: io::Cursor<Vec<u8>>,
    }
    impl Read for Quiet {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.timeouts > 0 {
                self.timeouts -= 1;
                return Err(io::Error::new(io::ErrorKind::TimedOut, "quiet"));
            }
            self.data.read(buf)
        }
    }
    impl Write for Quiet {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_quiet_rdma_connection_is_waited_on_until_the_node_stops() {
        let stop = AtomicBool::new(false);
        let mut quiet = Quiet { timeouts: 50, data: io::Cursor::new(vec![7]) };
        let mut idle = IdleRdma::new(&mut quiet, &stop);
        let mut b = [0u8; 1];
        // Fifty quiet wake-ups between requests are not an error.
        assert_eq!(idle.read(&mut b).unwrap(), 1);
        assert_eq!(b[0], 7);

        let mut quiet = Quiet { timeouts: usize::MAX, data: io::Cursor::new(Vec::new()) };
        let mut idle = IdleRdma::new(&mut quiet, &stop);
        stop.store(true, Ordering::SeqCst);
        let e = idle.read(&mut b).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionAborted);
    }

    /// Without the operator switch the node declines the upgrade and the client carries on over
    /// TCP, counting the failed attempt.
    #[cfg(all(feature = "rdma", target_os = "linux"))]
    #[test]
    fn use_rdma_falls_back_to_tcp_when_the_node_has_it_off() {
        if rdma_enabled() {
            return;
        }
        let (_dir, node) = start_node();
        let cfg = RdmaConfig::new(crate::rdma_link::verbs_transport().unwrap(), 1);
        let client = RemoteDevice::new(node.local_addr(), Duration::from_secs(5))
            .use_rdma(cfg)
            .unwrap();
        let off = client.append(1, b"hello").unwrap();
        assert_eq!(client.read_exact_at(off, 5).unwrap(), b"hello");
        let s = client.rdma_stats().unwrap();
        assert_eq!((s.connect_failures, s.requests), (1, 0), "{s:?}");
    }

    /// An in-memory connection: reads come from `input`, writes are collected in `output`.
    struct Memory {
        input: io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Memory {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Memory {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn empty_shared(root: &Path) -> Shared {
        Shared {
            devices: Vec::new(),
            root: root.to_path_buf(),
            fences: Mutex::new(BTreeMap::new()),
            tls: None,
            id: String::new(),
            grant_key: None,
            rdma: RwLock::new(None),
            stop: AtomicBool::new(false),
            stats: Stats::default(),
            conns: Mutex::new(BTreeMap::new()),
            next_conn: AtomicU64::new(0),
            handlers: Mutex::new(Vec::new()),
        }
    }

    /// Sends an `RdmaUpgrade` request through the request loop and returns what the node answered and
    /// whether it agreed to switch transports.
    fn ask_for_upgrade(allow_upgrade: bool) -> (Response, Option<u32>) {
        let dir = tempfile::tempdir().unwrap();
        let mut input = Vec::new();
        write_message(&mut input, &Request::RdmaUpgrade { gid_index: 1 }, &[]).unwrap();
        let mut stream = Memory {
            input: io::Cursor::new(input),
            output: Vec::new(),
        };
        let outcome = serve_requests(&mut stream, &empty_shared(dir.path()), allow_upgrade);
        let resp = read_header::<Response>(&mut stream.output.as_slice()).unwrap();
        (resp, outcome)
    }

    /// `serve` passes `allow_upgrade = false` for a TLS connection (and for one already upgraded):
    /// RDMA carries no encryption, so the node must answer with an error and keep the connection on TLS.
    #[test]
    fn an_rdma_upgrade_is_refused_when_the_connection_may_not_upgrade() {
        let (resp, outcome) = ask_for_upgrade(false);
        assert_eq!(outcome, None, "the connection must not switch transports");
        match resp {
            Response::Error { message } => assert!(message.contains("not available on this connection"), "{message}"),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    /// Without `ATLAS_NATIVE_RDMA=1` even a plain connection is refused: the upgrade is opt-in.
    #[test]
    fn an_rdma_upgrade_is_refused_unless_the_node_enabled_it() {
        if rdma_enabled() {
            return; // the test host opted in; this case is about the default
        }
        let (resp, outcome) = ask_for_upgrade(true);
        assert_eq!(outcome, None);
        match resp {
            Response::Error { message } => assert!(message.contains("not enabled"), "{message}"),
            other => panic!("expected an error, got {other:?}"),
        }
    }
}
