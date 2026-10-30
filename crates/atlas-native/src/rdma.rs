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

use std::io;
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
}
