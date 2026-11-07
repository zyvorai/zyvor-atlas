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
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender},
        Arc,
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
}

/// Counters a client keeps about its RDMA use, shared across the devices of one mount.
#[derive(Debug, Default)]
pub struct RdmaStats {
    connects: AtomicU64,
    connect_failures: AtomicU64,
    tcp_fallback_connects: AtomicU64,
    midflight_errors: AtomicU64,
    requests: AtomicU64,
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
}

impl RdmaStats {
    pub fn snapshot(&self) -> RdmaSnapshot {
        RdmaSnapshot {
            connects: self.connects.load(Ordering::Relaxed),
            connect_failures: self.connect_failures.load(Ordering::Relaxed),
            tcp_fallback_connects: self.tcp_fallback_connects.load(Ordering::Relaxed),
            midflight_errors: self.midflight_errors.load(Ordering::Relaxed),
            requests: self.requests.load(Ordering::Relaxed),
        }
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

/// How long RDMA is not attempted on a device after the transport failed.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);
/// How long RDMA is not attempted after the node itself said it does not offer it.
pub const NODE_REFUSED_RETRY_AFTER: Duration = Duration::from_secs(300);

/// Client-side RDMA settings: the transport, the GID table index and where to count.
#[derive(Clone)]
pub struct RdmaConfig {
    pub transport: Arc<dyn RdmaTransport>,
    /// GID table entry of the local port (RoCE routes by GID; see [`crate::rdma`]).
    pub gid_index: u32,
    pub stats: Arc<RdmaStats>,
    /// Back-off after a transport failure before RDMA is attempted again on that device.
    pub retry_after: Duration,
}

impl RdmaConfig {
    pub fn new(transport: Arc<dyn RdmaTransport>, gid_index: u32) -> Self {
        Self {
            transport,
            gid_index,
            stats: Arc::new(RdmaStats::default()),
            retry_after: DEFAULT_RETRY_AFTER,
        }
    }
}

impl std::fmt::Debug for RdmaConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RdmaConfig")
            .field("transport", &self.transport)
            .field("gid_index", &self.gid_index)
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
            f(s);
            Ok(())
        })
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
        let fail = self.fail_connect.load(Ordering::SeqCst);
        let mut inner = Breakable {
            inner: control,
            broken: self.break_streams.clone(),
        };
        let worker = spawn_worker("atlas-fake-rdma-conn", move |rx, mut ready| {
            if fail {
                // Setup fails on the owner thread, as a verbs failure would.
                ready.fail(io::Error::other("fake RDMA connect failure"));
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
