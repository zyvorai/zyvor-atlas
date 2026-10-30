// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! RDMA building blocks over `libibverbs` (feature `rdma`, Linux only).
//!
//! This is the foundation for an RDMA transport between data nodes and clients, not the transport
//! itself: today the data path is TCP/TLS (`data_node`, `tls`) and nothing here changes that. What
//! exists is a way to list RDMA devices and one self-connected reliable-connection round trip, so
//! the verbs stack, the device and the build can be checked on a host before a transport is
//! written on top. It runs on SoftRoCE (`rdma link add rxe0 type rxe netdev lo`) as well as on
//! hardware, so a passing round trip on SoftRoCE says the plumbing works, never how fast it is.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// How long a round trip may take before it is reported as timed out.
const ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(5);

/// Names of the RDMA devices `libibverbs` can see (for example `rxe0`, `mlx5_0`). Empty when the
/// kernel has no RDMA device or the provider library is missing.
pub fn device_names() -> io::Result<Vec<String>> {
    let list = ibverbs::devices()?;
    Ok(list
        .iter()
        .map(|d| {
            d.name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect())
}

/// Sends `payload` over a reliable-connection queue pair connected to itself on the first RDMA
/// device and returns what the receive side got. `gid_index` selects the port's GID table entry
/// (RoCE routes by GID; on SoftRoCE over loopback index 1 is usually the IPv4 one, but check
/// `rdma link` / `show_gids` on the host).
pub fn loopback_roundtrip(payload: &[u8], gid_index: u32) -> io::Result<Vec<u8>> {
    let n = payload.len();
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty payload"));
    }
    let devices = ibverbs::devices()?;
    let device = devices
        .iter()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no RDMA device available"))?;
    let ctx = device.open()?;
    let cq = ctx.create_cq(16, 0)?;
    let pd = ctx.alloc_pd()?;
    let prepared = pd
        .create_qp(&cq, &cq, ibverbs::ibv_qp_type::IBV_QPT_RC)
        .set_gid_index(gid_index)
        .build()?;
    let endpoint = prepared.endpoint();
    let mut qp = prepared.handshake(endpoint)?;

    // One registered buffer: the first n bytes receive, the last n bytes are sent.
    let mut mr = pd.allocate::<u8>(2 * n)?;
    mr[n..].copy_from_slice(payload);
    const SEND: u64 = 1;
    const RECV: u64 = 2;
    // Safety: `mr` outlives both work requests, which complete before this function returns.
    unsafe { qp.post_receive(&mut mr, ..n, RECV) }?;
    unsafe { qp.post_send(&mut mr, n.., SEND) }?;

    let (mut sent, mut received) = (false, false);
    let mut completions = [ibverbs::ibv_wc::default(); 16];
    let started = Instant::now();
    while !sent || !received {
        if started.elapsed() > ROUND_TRIP_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "RDMA round trip timed out",
            ));
        }
        for wc in cq.poll(&mut completions[..])? {
            match wc.wr_id() {
                SEND => sent = true,
                RECV => received = true,
                other => {
                    return Err(io::Error::other(format!("unexpected completion {other}")))
                }
            }
        }
    }
    Ok(mr[..n].to_vec())
}

/// Size of one RDMA message. A write larger than this is split; a read never spans messages.
const MSG: usize = 64 * 1024;
/// Receive buffers kept posted. The sender is not credit-limited: with infinite RNR retry, a send
/// that finds no posted receive is retried by the adapter until one appears, which is the
/// back-pressure.
const DEPTH: usize = 16;
/// Work request id of the (single, synchronous) send; receive slots use their slot index.
const SEND_ID: u64 = 1 << 32;
/// How long a read or write may wait for the peer.
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// A reliable byte stream over one RDMA reliable-connection queue pair: `Read` and `Write` like
/// `TcpStream`, so protocol code written against those traits can run over it.
///
/// Created only inside [`with_stream`], because the verbs objects borrow from one another and are
/// kept on that function's stack. That suits a server handling one connection per thread; a client
/// that pools long-lived connections needs the resources owned together (a self-referential
/// struct, or a worker thread per connection), which this does not provide.
///
/// Synchronous by design: one send in flight, completions busy-polled. It is a correctness
/// building block, not a fast path.
pub struct RdmaStream<'a, 'r> {
    qp: &'a mut ibverbs::QueuePair<'r>,
    cq: &'a ibverbs::CompletionQueue<'r>,
    mr: &'a mut ibverbs::MemoryRegion<u8>,
    /// Received messages not yet handed out: (slot, length).
    ready: VecDeque<(usize, usize)>,
    /// Bytes of the front message already consumed by `read`.
    consumed: usize,
    send_done: bool,
}

impl RdmaStream<'_, '_> {
    /// Polls the completion queue once, sorting completions into `ready` / `send_done`.
    fn pump(&mut self) -> io::Result<()> {
        let mut wcs = [ibverbs::ibv_wc::default(); 16];
        for wc in self.cq.poll(&mut wcs[..])? {
            if let Some((status, _vendor)) = wc.error() {
                return Err(io::Error::other(format!("RDMA work request failed: {status:?}")));
            }
            let id = wc.wr_id();
            if id == SEND_ID {
                self.send_done = true;
            } else {
                self.ready.push_back((id as usize, wc.len()));
            }
        }
        Ok(())
    }

    fn repost(&mut self, slot: usize) -> io::Result<()> {
        // Safety: the registered buffer outlives the work request (it lives as long as `self`).
        unsafe { self.qp.post_receive(self.mr, slot * MSG..(slot + 1) * MSG, slot as u64) }
    }
}

impl Read for RdmaStream<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let started = Instant::now();
        while self.ready.is_empty() {
            self.pump()?;
            if self.ready.is_empty() {
                if started.elapsed() > STREAM_TIMEOUT {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "RDMA read timed out"));
                }
                std::thread::yield_now();
            }
        }
        let (slot, len) = self.ready[0];
        let n = buf.len().min(len - self.consumed);
        let from = slot * MSG + self.consumed;
        buf[..n].copy_from_slice(&self.mr[from..from + n]);
        self.consumed += n;
        if self.consumed == len {
            self.ready.pop_front();
            self.consumed = 0;
            self.repost(slot)?;
        }
        Ok(n)
    }
}

impl Write for RdmaStream<'_, '_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let n = data.len().min(MSG);
        let base = DEPTH * MSG;
        self.mr[base..base + n].copy_from_slice(&data[..n]);
        self.send_done = false;
        // Safety: the staging slot is not touched again until this send completes, below.
        unsafe { self.qp.post_send(self.mr, base..base + n, SEND_ID) }?;
        let started = Instant::now();
        while !self.send_done {
            self.pump()?;
            if !self.send_done {
                if started.elapsed() > STREAM_TIMEOUT {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "RDMA write timed out"));
                }
                std::thread::yield_now();
            }
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Connects a queue pair to the peer at the other end of `control` and runs `f` over the RDMA
/// stream. Both ends call this on their ends of the same TCP connection; the TCP connection only
/// carries the one-time exchange of queue-pair endpoints (JSON) and is left untouched afterwards.
/// `gid_index` selects the GID table entry, as in [`loopback_roundtrip`].
pub fn with_stream<R>(
    control: &mut TcpStream,
    gid_index: u32,
    f: impl FnOnce(&mut RdmaStream<'_, '_>) -> io::Result<R>,
) -> io::Result<R> {
    let devices = ibverbs::devices()?;
    let device = devices
        .iter()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no RDMA device available"))?;
    let ctx = device.open()?;
    let cq = ctx.create_cq((2 * DEPTH) as i32, 0)?;
    let pd = ctx.alloc_pd()?;
    let prepared = pd
        .create_qp(&cq, &cq, ibverbs::ibv_qp_type::IBV_QPT_RC)
        .set_gid_index(gid_index)
        // The queue pair defaults to one outstanding work request per queue; we keep `DEPTH`
        // receives posted and one send in flight.
        .set_max_recv_wr(DEPTH as u32)
        .set_max_send_wr(2)
        // 7 = retry indefinitely when the peer has no receive posted yet.
        .set_rnr_retry(7)
        .build()?;

    // Both ends write their endpoint and then read the peer's; the messages are tiny, so the
    // writes cannot block on each other.
    let mine = serde_json::to_vec(&prepared.endpoint()).map_err(io::Error::other)?;
    control.write_all(&(mine.len() as u32).to_be_bytes())?;
    control.write_all(&mine)?;
    let mut len = [0u8; 4];
    control.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > 4096 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad RDMA endpoint message"));
    }
    let mut theirs = vec![0u8; len];
    control.read_exact(&mut theirs)?;
    let remote: ibverbs::QueuePairEndpoint =
        serde_json::from_slice(&theirs).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut qp = prepared.handshake(remote)?;

    let mut mr = pd.allocate::<u8>((DEPTH + 1) * MSG)?;
    let mut stream = RdmaStream {
        qp: &mut qp,
        cq: &cq,
        mr: &mut mr,
        ready: VecDeque::new(),
        consumed: 0,
        send_done: false,
    };
    for slot in 0..DEPTH {
        stream.repost(slot)?;
    }
    f(&mut stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs an RDMA device (SoftRoCE is enough): `cargo test -p atlas-native --features rdma
    /// -- --ignored rdma`. Ignored by default so CI hosts without one stay green.
    #[test]
    #[ignore = "needs an RDMA device (e.g. SoftRoCE rxe0)"]
    fn rdma_loopback_roundtrip_returns_the_payload() {
        let names = device_names().expect("list RDMA devices");
        assert!(!names.is_empty(), "no RDMA device visible to libibverbs");
        let gid: u32 = std::env::var("ATLAS_RDMA_GID_INDEX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let got = loopback_roundtrip(&payload, gid).expect("round trip");
        assert_eq!(got, payload);
    }

    /// Two separate endpoints (a client and a server thread, each with its own context and queue
    /// pair) exchange 1 MiB each way over the SoftRoCE device, through `Read`/`Write`.
    #[test]
    #[ignore = "needs an RDMA device (e.g. SoftRoCE rxe0)"]
    fn two_endpoints_exchange_a_megabyte_each_way() {
        use std::net::TcpListener;
        let gid: u32 = std::env::var("ATLAS_RDMA_GID_INDEX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let size = 1 << 20;
        let ping: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let pong: Vec<u8> = ping.iter().map(|b| b ^ 0xFF).collect();

        let (expect_in, send_back) = (ping.clone(), pong.clone());
        let server = std::thread::spawn(move || {
            let (mut tcp, _) = listener.accept().unwrap();
            with_stream(&mut tcp, gid, |s| {
                let mut got = vec![0u8; size];
                s.read_exact(&mut got)?;
                assert_eq!(got, expect_in, "server saw the client's bytes");
                s.write_all(&send_back)
            })
        });
        let mut tcp = TcpStream::connect(addr).unwrap();
        let reply = with_stream(&mut tcp, gid, |s| {
            s.write_all(&ping)?;
            let mut got = vec![0u8; size];
            s.read_exact(&mut got)?;
            Ok(got)
        })
        .expect("client");
        server.join().unwrap().expect("server");
        assert_eq!(reply, pong, "client saw the server's bytes");
    }
}
