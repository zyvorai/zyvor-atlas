// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! The transport-independent half of RDMA support: what a pooled RDMA connection is, how a client
//! decides to use one and falls back to TCP, and a fake transport for testing all of that on any
//! OS without RDMA hardware.
//!
//! [`crate::rdma`] (feature `rdma`, Linux) provides the verbs-backed stream, whose objects borrow
//! from one another and so only live inside a function call. [`WorkerStream`] solves that for
//! clients: one worker thread per connection owns the verbs objects for the connection's whole
//! life and the pooled handle talks to it over channels. Nothing in this module needs libibverbs.
//!
//! RDMA here is a plaintext transport for the same request/response protocol as TCP (a switch
//! negotiated with `RdmaUpgrade` over an ordinary connection). It is therefore refused when TLS is
//! configured; see [`crate::RemoteDevice::use_rdma`].

use std::{
    io::{self, Read, Write},
    net::TcpStream,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

/// A byte stream the request loop can run over (no `Send` needed: verbs objects are not).
pub trait Wire: Read + Write {}
impl<T: Read + Write + ?Sized> Wire for T {}

/// A byte stream that can be moved to another thread, which is what a connection pool needs.
pub trait Duplex: Read + Write + Send {}
impl<T: Read + Write + Send + ?Sized> Duplex for T {}

/// How RDMA connections come to exist. `control` is the ordinary TCP connection over which the
/// two ends already agreed to switch; the transport uses it for its own setup exchange.
pub trait RdmaTransport: Send + Sync + std::fmt::Debug {
    /// Server side: runs `f` over the RDMA stream for as long as the connection lasts.
    fn serve(
        &self,
        control: &mut TcpStream,
        gid_index: u32,
        f: &mut dyn FnMut(&mut dyn Wire),
    ) -> io::Result<()>;

    /// Client side: a long-lived stream for the connection pool. Takes the control connection
    /// (the transport may keep it open for as long as the stream lives).
    fn connect(&self, control: TcpStream, gid_index: u32) -> io::Result<Box<dyn Duplex>>;

    /// Memory one connection pins (registered, so counted against `RLIMIT_MEMLOCK`) on each end.
    /// Zero for a transport that pins nothing (the fake one), which is then not memlock-capped.
    fn pinned_bytes_per_connection(&self) -> u64 {
        0
    }
}

/// Size of one RDMA message of the verbs stream. A write larger than this is split.
pub(crate) const MSG: usize = 64 * 1024;
/// Receive buffers kept posted per verbs connection.
pub(crate) const DEPTH: usize = 16;
/// Bytes one verbs connection registers: `DEPTH` receive slots plus one send slot.
pub const VERBS_PINNED_BYTES: u64 = ((DEPTH + 1) * MSG) as u64;

/// The soft `RLIMIT_MEMLOCK` of a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Memlock {
    Unlimited,
    Bytes(u64),
}

/// Parses the soft "Max locked memory" limit out of the text of `/proc/<pid>/limits`.
pub fn parse_memlock_soft(limits: &str) -> Option<Memlock> {
    let rest = limits
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("Max locked memory"))?;
    let soft = rest.split_whitespace().next()?;
    if soft == "unlimited" {
        return Some(Memlock::Unlimited);
    }
    soft.parse().ok().map(Memlock::Bytes)
}

/// This process's soft `RLIMIT_MEMLOCK`, read from `/proc/self/limits`; `None` where that file does
/// not exist or cannot be parsed.
pub fn soft_memlock() -> Option<Memlock> {
    parse_memlock_soft(&std::fs::read_to_string("/proc/self/limits").ok()?)
}

/// Connection cap used when the limit is unlimited or unknown, or the transport pins nothing.
pub const DEFAULT_MAX_CONNECTIONS: usize = 64;

/// Below this soft `RLIMIT_MEMLOCK` a warning is recorded when RDMA is enabled.
pub const MEMLOCK_WARN_BELOW: u64 = 64 << 20;

/// How many RDMA connections to allow given the memlock limit: half the limit divided by what one
/// connection pins (the other half is left for the peer end when both ends share the process, and
/// for everything else that locks memory), at least 1. `None`, unlimited or a transport that pins
/// nothing give [`DEFAULT_MAX_CONNECTIONS`].
pub fn default_max_connections(memlock: Option<Memlock>, pinned_per_connection: u64) -> usize {
    match memlock {
        Some(Memlock::Bytes(limit)) if pinned_per_connection > 0 => {
            usize::try_from((limit / 2) / pinned_per_connection)
                .unwrap_or(usize::MAX)
                .max(1)
        }
        _ => DEFAULT_MAX_CONNECTIONS,
    }
}

/// What to do about `ENOMEM` while setting up RDMA.
pub const ENOMEM_HINT: &str = "RDMA setup failed with ENOMEM (os error 12): registering memory \
    exceeded RLIMIT_MEMLOCK. Raise the locked-memory limit of this process (systemd \
    LimitMEMLOCK=infinity, `ulimit -l unlimited`, a container's --ulimit memlock=-1:-1 or the \
    IPC_LOCK capability); a rootless container cannot exceed its host hard limit. Until then \
    connections over the cap use TCP";

/// Whether `e` is the kernel refusing to pin memory.
pub fn is_enomem(e: &io::Error) -> bool {
    e.raw_os_error() == Some(12)
}

/// A warning when the soft memlock limit is low enough that RDMA connections will be capped hard.
pub fn memlock_warning() -> Option<String> {
    match soft_memlock()? {
        Memlock::Bytes(b) if b < MEMLOCK_WARN_BELOW => Some(format!(
            "RLIMIT_MEMLOCK is {} KiB: every RDMA connection pins {} KiB, so only a few fit and the \
             rest use TCP. Raise it (systemd LimitMEMLOCK=infinity, `ulimit -l unlimited`, a \
             container's --ulimit memlock=-1:-1 or IPC_LOCK); a rootless container cannot exceed \
             its host hard limit",
            b / 1024,
            VERBS_PINNED_BYTES / 1024
        )),
        _ => None,
    }
}

/// Counters a client keeps about its RDMA use, shared across the devices of one mount.
#[derive(Debug, Default)]
pub struct RdmaStats {
    connects: AtomicU64,
    connect_failures: AtomicU64,
    tcp_fallback_connects: AtomicU64,
    midflight_errors: AtomicU64,
    requests: AtomicU64,
    capped_fallbacks: AtomicU64,
    enomem_failures: AtomicU64,
    /// RDMA connections alive now (reserved before connecting, released when the link is dropped).
    live: AtomicUsize,
    /// Diagnostics seen so far, each kept once.
    notes: Mutex<Vec<String>>,
}

/// A point-in-time copy of [`RdmaStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RdmaSnapshot {
    /// RDMA connections established.
    pub connects: u64,
    /// Attempts to establish one that failed (the node refused, or the transport failed).
    pub connect_failures: u64,
    /// TCP connections opened instead of RDMA ones, after a failed attempt or during back-off.
    pub tcp_fallback_connects: u64,
    /// Requests that failed on an established RDMA connection and were retried on TCP.
    pub midflight_errors: u64,
    /// Requests completed over RDMA.
    pub requests: u64,
    /// Connections opened over TCP because the live-connection cap was reached. Not a failure:
    /// nothing is retried and RDMA is not backed off.
    pub capped_fallbacks: u64,
    /// Failed RDMA setups whose cause was `ENOMEM` (the memlock limit), a subset of
    /// `connect_failures`.
    pub enomem_failures: u64,
    /// RDMA connections alive right now (pooled or in use).
    pub live_connections: u64,
}

impl RdmaStats {
    pub fn snapshot(&self) -> RdmaSnapshot {
        RdmaSnapshot {
            connects: self.connects.load(Ordering::Relaxed),
            connect_failures: self.connect_failures.load(Ordering::Relaxed),
            tcp_fallback_connects: self.tcp_fallback_connects.load(Ordering::Relaxed),
            midflight_errors: self.midflight_errors.load(Ordering::Relaxed),
            requests: self.requests.load(Ordering::Relaxed),
            capped_fallbacks: self.capped_fallbacks.load(Ordering::Relaxed),
            enomem_failures: self.enomem_failures.load(Ordering::Relaxed),
            live_connections: self.live.load(Ordering::Relaxed) as u64,
        }
    }

    /// Diagnostics recorded so far (memlock warnings, `ENOMEM` explanations), each once.
    pub fn diagnostics(&self) -> Vec<String> {
        self.notes.lock().map(|n| n.clone()).unwrap_or_default()
    }

    /// Records `msg` unless already recorded; true when it is new.
    fn note_once(&self, msg: &str) -> bool {
        match self.notes.lock() {
            Ok(mut n) if !n.iter().any(|m| m == msg) => {
                n.push(msg.to_string());
                true
            }
            _ => false,
        }
    }

    /// Reserves one of at most `cap` live connections; the slot is released when dropped.
    pub(crate) fn reserve(self: &Arc<Self>, cap: usize) -> Option<ConnSlot> {
        self.live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < cap).then_some(n + 1))
            .ok()
            .map(|_| ConnSlot(self.clone()))
    }
    pub(crate) fn capped_fallback(&self) {
        self.capped_fallbacks.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn enomem(&self) {
        self.enomem_failures.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn connected(&self) {
        self.connects.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn connect_failed(&self) {
        self.connect_failures.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn tcp_fallback(&self) {
        self.tcp_fallback_connects.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn midflight_error(&self) {
        self.midflight_errors.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }
}

/// One live RDMA connection counted against the cap; released on drop.
pub(crate) struct ConnSlot(Arc<RdmaStats>);

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How long RDMA is not attempted on a device after the transport failed.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);
/// How long RDMA is not attempted after the node itself said it does not offer it.
pub const NODE_REFUSED_RETRY_AFTER: Duration = Duration::from_secs(300);

/// Receives each new RDMA diagnostic.
pub type DiagnosticSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Client-side RDMA settings: the transport, the GID table index and where to count.
#[derive(Clone)]
pub struct RdmaConfig {
    pub transport: Arc<dyn RdmaTransport>,
    /// GID table entry of the local port (RoCE routes by GID; see [`crate::rdma`]).
    pub gid_index: u32,
    pub stats: Arc<RdmaStats>,
    /// Back-off after a transport failure before RDMA is attempted again on that device.
    pub retry_after: Duration,
    /// Most RDMA connections alive at once across every device sharing this config's `stats`
    /// (clones of one config share them). Beyond it a new connection is opened over TCP without
    /// counting a failure or backing off. The default follows the soft `RLIMIT_MEMLOCK`, see
    /// [`default_max_connections`].
    pub max_connections: usize,
    /// Called once per new diagnostic (see [`RdmaStats::diagnostics`]), e.g. to log it.
    pub diagnostic_sink: Option<DiagnosticSink>,
}

impl RdmaConfig {
    pub fn new(transport: Arc<dyn RdmaTransport>, gid_index: u32) -> Self {
        let max_connections =
            default_max_connections(soft_memlock(), transport.pinned_bytes_per_connection());
        Self {
            transport,
            gid_index,
            stats: Arc::new(RdmaStats::default()),
            retry_after: DEFAULT_RETRY_AFTER,
            max_connections,
            diagnostic_sink: None,
        }
    }

    /// Overrides the live-connection cap (at least 1).
    pub fn with_max_connections(mut self, n: usize) -> Self {
        self.max_connections = n.max(1);
        self
    }

    /// Sends each new diagnostic to `sink`, including the ones already recorded.
    pub fn with_diagnostic_sink(mut self, sink: impl Fn(&str) + Send + Sync + 'static) -> Self {
        let sink: DiagnosticSink = Arc::new(sink);
        for m in self.stats.diagnostics() {
            sink(&m);
        }
        self.diagnostic_sink = Some(sink);
        self
    }

    /// Records [`memlock_warning`] (once) when this config's transport pins memory and the soft
    /// limit is low. Called by `RemoteDevice::use_rdma`; callable earlier, e.g. at startup.
    pub fn warn_if_memlock_low(&self) {
        if self.transport.pinned_bytes_per_connection() > 0 {
            if let Some(w) = memlock_warning() {
                self.note(&w);
            }
        }
    }

    /// Records a diagnostic once and hands it to the sink the first time.
    pub(crate) fn note(&self, msg: &str) {
        if self.stats.note_once(msg) {
            if let Some(sink) = &self.diagnostic_sink {
                sink(msg);
            }
        }
    }
}

impl std::fmt::Debug for RdmaConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RdmaConfig")
            .field("transport", &self.transport)
            .field("gid_index", &self.gid_index)
            .field("max_connections", &self.max_connections)
            .finish()
    }
}

/// The verbs-backed transport when this build has one (feature `rdma` on Linux), else `None`.
pub fn verbs_transport() -> Option<Arc<dyn RdmaTransport>> {
    #[cfg(all(feature = "rdma", target_os = "linux"))]
    {
        Some(Arc::new(VerbsTransport))
    }
    #[cfg(not(all(feature = "rdma", target_os = "linux")))]
    {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Worker-owned streams

/// A request to the thread that owns a stream.
pub(crate) enum Cmd {
    Write(Vec<u8>, SyncSender<io::Result<()>>),
    Read(usize, SyncSender<io::Result<Vec<u8>>>),
}

/// Runs the commands of one [`WorkerStream`] against `s` until the handle is dropped or `s`
/// fails. Called on the thread that owns the stream.
pub(crate) fn run_commands<S: Read + Write + ?Sized>(rx: Receiver<Cmd>, s: &mut S) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Write(data, reply) => {
                let r = s.write_all(&data).and_then(|()| s.flush());
                let failed = r.is_err();
                let _ = reply.send(r);
                if failed {
                    return;
                }
            }
            Cmd::Read(n, reply) => {
                let mut buf = vec![0u8; n];
                let r = s.read(&mut buf).map(|got| {
                    buf.truncate(got);
                    buf
                });
                let failed = r.is_err();
                let _ = reply.send(r);
                if failed {
                    return;
                }
            }
        }
    }
}

/// Handed to a worker body so it can report whether the stream came up.
pub(crate) struct Ready(Option<SyncSender<io::Result<()>>>);

impl Ready {
    /// The stream is established.
    pub(crate) fn ok(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(Ok(()));
        }
    }
    /// Setup failed; a no-op once [`Self::ok`] was called.
    pub(crate) fn fail(&mut self, e: io::Error) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(Err(e));
        }
    }
}

/// How long [`spawn_worker`] waits for the stream to come up.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

/// A stream owned by a dedicated thread, usable from any thread through `Read` and `Write`.
///
/// The owner thread keeps non-`Send`, self-referential resources (verbs contexts, queue pairs,
/// registered memory) alive for the connection's lifetime; dropping the handle ends it. Every
/// `read` or `write` is one round trip to the worker.
pub struct WorkerStream {
    tx: Sender<Cmd>,
    thread: Option<thread::JoinHandle<()>>,
}

/// Starts the owner thread. `body` runs on it, builds the stream, calls `ready.ok()` and then
/// [`run_commands`]; if it returns first, `ready` reports why (or that it ended early).
pub(crate) fn spawn_worker(
    name: &str,
    body: impl FnOnce(Receiver<Cmd>, Ready) + Send + 'static,
) -> io::Result<WorkerStream> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let handle = thread::Builder::new()
        .name(name.to_string())
        .spawn(move || body(rx, Ready(Some(ready_tx))))?;
    match ready_rx.recv_timeout(SETUP_TIMEOUT) {
        Ok(Ok(())) => Ok(WorkerStream {
            tx,
            thread: Some(handle),
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "stream setup did not finish",
        )),
    }
}

fn gone() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "stream owner thread has ended")
}

impl Read for WorkerStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let (reply, rx) = mpsc::sync_channel(1);
        self.tx
            .send(Cmd::Read(buf.len(), reply))
            .map_err(|_| gone())?;
        let data = rx.recv().map_err(|_| gone())??;
        buf[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }
}

impl Write for WorkerStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let (reply, rx) = mpsc::sync_channel(1);
        self.tx
            .send(Cmd::Write(data.to_vec(), reply))
            .map_err(|_| gone())?;
        rx.recv().map_err(|_| gone())??;
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for WorkerStream {
    fn drop(&mut self) {
        // Closing the command channel ends the worker's loop; wait so the verbs objects are
        // released before the handle is gone. A worker stuck in a long read is detached instead.
        let (dead, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.tx, dead));
        if let Some(t) = self.thread.take() {
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Verbs transport

/// How often a served verbs stream wakes from a quiet read.
#[cfg(all(feature = "rdma", target_os = "linux"))]
const SERVE_IDLE_POLL: Duration = Duration::from_millis(500);

/// The transport over `libibverbs` queue pairs; one worker thread per client connection.
#[cfg(all(feature = "rdma", target_os = "linux"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct VerbsTransport;

#[cfg(all(feature = "rdma", target_os = "linux"))]
impl RdmaTransport for VerbsTransport {
    fn serve(
        &self,
        control: &mut TcpStream,
        gid_index: u32,
        f: &mut dyn FnMut(&mut dyn Wire),
    ) -> io::Result<()> {
        crate::rdma::with_stream(control, gid_index, |s| {
            // An idle connection is normal: reads wake up often so the caller can notice a
            // shutdown, and decide itself whether a quiet stream is dead.
            s.set_read_timeout(SERVE_IDLE_POLL);
            f(s);
            Ok(())
        })
    }

    fn pinned_bytes_per_connection(&self) -> u64 {
        VERBS_PINNED_BYTES
    }

    fn connect(&self, mut control: TcpStream, gid_index: u32) -> io::Result<Box<dyn Duplex>> {
        let worker = spawn_worker("atlas-rdma-conn", move |rx, mut ready| {
            let r = crate::rdma::with_stream(&mut control, gid_index, |s| {
                ready.ok();
                run_commands(rx, s);
                Ok(())
            });
            if let Err(e) = r {
                ready.fail(e);
            }
        })?;
        Ok(Box::new(worker))
    }
}

// ---------------------------------------------------------------------------------------------
// Fake transport

/// A transport for tests on any OS: the "RDMA stream" is the control TCP connection itself, with
/// faults on demand. Clients get theirs through a [`WorkerStream`] so the pooled-handle machinery
/// (channels, owner thread) is exercised exactly as with verbs. It proves the pooling, fallback,
/// grant and counter logic; it says nothing about RDMA itself.
#[derive(Debug, Default)]
pub struct FakeTransport {
    /// Fail every client `connect`.
    pub fail_connect: AtomicBool,
    /// Fail every client `connect` with `ENOMEM`, as registering memory over the memlock limit does.
    pub fail_connect_enomem: AtomicBool,
    /// Make every established client stream fail its next read or write (then stay failed).
    pub break_streams: Arc<AtomicBool>,
    /// Client streams opened.
    pub client_opens: AtomicU64,
    /// Server streams served.
    pub server_opens: AtomicU64,
}

impl FakeTransport {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

/// Fails reads and writes once `broken` is set.
struct Breakable {
    inner: TcpStream,
    broken: Arc<AtomicBool>,
}

fn broken_pipe() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionReset, "fake RDMA stream broken")
}

impl Read for Breakable {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.broken.load(Ordering::SeqCst) {
            return Err(broken_pipe());
        }
        self.inner.read(buf)
    }
}

impl Write for Breakable {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.broken.load(Ordering::SeqCst) {
            return Err(broken_pipe());
        }
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl RdmaTransport for FakeTransport {
    fn serve(
        &self,
        control: &mut TcpStream,
        _gid_index: u32,
        f: &mut dyn FnMut(&mut dyn Wire),
    ) -> io::Result<()> {
        self.server_opens.fetch_add(1, Ordering::SeqCst);
        f(control);
        Ok(())
    }

    fn connect(&self, control: TcpStream, _gid_index: u32) -> io::Result<Box<dyn Duplex>> {
        let enomem = self.fail_connect_enomem.load(Ordering::SeqCst);
        let fail = enomem || self.fail_connect.load(Ordering::SeqCst);
        let mut inner = Breakable {
            inner: control,
            broken: self.break_streams.clone(),
        };
        let worker = spawn_worker("atlas-fake-rdma-conn", move |rx, mut ready| {
            if fail {
                // Setup fails on the owner thread, as a verbs failure would.
                ready.fail(if enomem {
                    io::Error::from_raw_os_error(12)
                } else {
                    io::Error::other("fake RDMA connect failure")
                });
                return;
            }
            ready.ok();
            run_commands(rx, &mut inner);
        })?;
        self.client_opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(worker))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        (c, l.accept().unwrap().0)
    }

    const LIMITS: &str = "\
Limit                     Soft Limit           Hard Limit           Units     
Max cpu time              unlimited            unlimited            seconds   
Max locked memory         8388608              8388608              bytes     
Max address space         unlimited            unlimited            bytes     
";

    #[test]
    fn the_soft_memlock_limit_is_read_from_proc_limits_text() {
        assert_eq!(parse_memlock_soft(LIMITS), Some(Memlock::Bytes(8 << 20)));
        let unlimited = LIMITS.replace("8388608              8388608", "unlimited            unlimited");
        assert_eq!(parse_memlock_soft(&unlimited), Some(Memlock::Unlimited));
        // Soft and hard differ: the soft one counts.
        let soft_low = LIMITS.replace("8388608              8388608", "65536                unlimited");
        assert_eq!(parse_memlock_soft(&soft_low), Some(Memlock::Bytes(65536)));
        assert_eq!(parse_memlock_soft("Max cpu time unlimited unlimited seconds\n"), None);
        assert_eq!(parse_memlock_soft("Max locked memory  lots  lots bytes\n"), None);
        assert_eq!(parse_memlock_soft(""), None);
    }

    #[test]
    fn the_default_cap_follows_the_memlock_limit() {
        let per = VERBS_PINNED_BYTES;
        assert_eq!(per, 17 * 64 * 1024);
        // 8 MiB: half of it, 4 MiB, holds three 1.06 MiB connections.
        assert_eq!(default_max_connections(Some(Memlock::Bytes(8 << 20)), per), 3);
        assert_eq!(default_max_connections(Some(Memlock::Bytes(64 << 10)), per), 1);
        assert_eq!(default_max_connections(Some(Memlock::Bytes(0)), per), 1);
        assert_eq!(default_max_connections(Some(Memlock::Bytes(1 << 30)), per), 481);
        assert_eq!(default_max_connections(Some(Memlock::Unlimited), per), DEFAULT_MAX_CONNECTIONS);
        assert_eq!(default_max_connections(None, per), DEFAULT_MAX_CONNECTIONS);
        // A transport that pins nothing is not memlock-capped.
        assert_eq!(default_max_connections(Some(Memlock::Bytes(1)), 0), DEFAULT_MAX_CONNECTIONS);
    }

    #[test]
    fn a_slot_is_released_when_dropped_and_the_cap_holds() {
        let stats = Arc::new(RdmaStats::default());
        let a = stats.reserve(2).unwrap();
        let _b = stats.reserve(2).unwrap();
        assert!(stats.reserve(2).is_none());
        assert_eq!(stats.snapshot().live_connections, 2);
        drop(a);
        assert_eq!(stats.snapshot().live_connections, 1);
        assert!(stats.reserve(2).is_some());
    }

    #[test]
    fn a_diagnostic_is_recorded_and_sent_once() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink_seen = seen.clone();
        let cfg = RdmaConfig::new(FakeTransport::new(), 1)
            .with_diagnostic_sink(move |m| sink_seen.lock().unwrap().push(m.to_string()));
        cfg.note("a");
        cfg.note("a");
        cfg.note("b");
        assert_eq!(*seen.lock().unwrap(), ["a", "b"]);
        assert_eq!(cfg.stats.diagnostics(), ["a", "b"]);
    }

    #[test]
    fn a_worker_stream_carries_bytes_both_ways() {
        let (client, mut server) = pair();
        let t = FakeTransport::new();
        let mut s = t.connect(client, 1).unwrap();
        s.write_all(b"hello over the worker").unwrap();
        let mut got = [0u8; 21];
        server.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello over the worker");
        server.write_all(&[7u8; 100_000]).unwrap();
        let mut back = vec![0u8; 100_000];
        s.read_exact(&mut back).unwrap();
        assert!(back.iter().all(|b| *b == 7));
        assert_eq!(t.client_opens.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_broken_stream_reports_the_error_and_stays_broken() {
        let (client, _server) = pair();
        let t = FakeTransport::new();
        let mut s = t.connect(client, 1).unwrap();
        t.break_streams.store(true, Ordering::SeqCst);
        assert!(s.write_all(b"x").is_err());
        // The owner thread ended with the failure; later use is a clean error, not a hang.
        assert!(s.write_all(b"y").is_err());
        let mut b = [0u8; 1];
        assert!(s.read(&mut b).is_err());
    }

    #[test]
    fn a_failed_setup_is_an_error_from_connect() {
        let (client, _server) = pair();
        let t = FakeTransport::new();
        t.fail_connect.store(true, Ordering::SeqCst);
        assert!(t.connect(client, 1).is_err());
    }

    #[test]
    fn dropping_the_handle_ends_the_owner_thread() {
        let (client, _server) = pair();
        let (done_tx, done_rx) = mpsc::channel();
        let s = spawn_worker("test-owner", move |rx, mut ready| {
            let mut c = client;
            ready.ok();
            run_commands(rx, &mut c);
            let _ = done_tx.send(());
        })
        .unwrap();
        drop(s);
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("owner thread exits when the handle is dropped");
    }
}
