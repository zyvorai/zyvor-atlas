// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! The RDMA client path (pooled connections, fallback to TCP, counters, write grants) over
//! [`FakeTransport`], which needs no RDMA hardware and runs on any OS. These prove the pooling,
//! fallback and grant logic; they say nothing about verbs or RDMA performance.
//!
//! `pooled_client_over_verbs` (below, `rdma` feature, Linux, `#[ignore]`) is the same pooled
//! client over libibverbs. SoftRoCE is enough and an RDMA NIC is not required
//! (`rdma link add rxe0 type rxe netdev lo`, GID index 1 unless `ATLAS_RDMA_GID_INDEX` is set).
//! It is not CI and not a throughput test: a pass on `rxe0` checks that the pooled client
//! connects and bytes match. Run it with:
//! `cargo test -p atlas-native --features rdma -- --ignored pooled_client_over_verbs`.
//! See also the ignored device tests in `src/rdma.rs`. Pipelined sends and registered-memory
//! zero-copy are scaffolded in `rdma_pipeline` and are not used here.

use std::{
    net::TcpListener,
    sync::{atomic::Ordering, Arc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use atlas_native::{
    device::FileDevice, BlockStore, DataNodeServer, FakeTransport, GrantKey, NativeError,
    RdmaConfig, RemoteDevice,
};

const TIMEOUT: Duration = Duration::from_secs(5);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn node(
    dir: &std::path::Path,
    transport: Option<Arc<FakeTransport>>,
    key: Option<GrantKey>,
) -> DataNodeServer {
    let dev: Arc<dyn BlockStore> = Arc::new(FileDevice::open(dir.join("nvme0.data")).unwrap());
    let n = DataNodeServer::start_devices_with(
        "n1",
        dir,
        vec![dev],
        TcpListener::bind("127.0.0.1:0").unwrap(),
        None,
        key,
    )
    .unwrap();
    match transport {
        Some(t) => n.with_rdma_transport(t),
        None => n,
    }
}

fn client(n: &DataNodeServer, t: &Arc<FakeTransport>) -> RemoteDevice {
    let mut cfg = RdmaConfig::new(t.clone(), 1);
    cfg.retry_after = Duration::from_secs(60);
    RemoteDevice::new(n.local_addr(), TIMEOUT)
        .use_rdma(cfg)
        .unwrap()
}

fn bytes(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8 ^ seed).collect()
}

#[test]
fn requests_reuse_one_pooled_rdma_connection() {
    let dir = tempfile::tempdir().unwrap();
    let t = FakeTransport::new();
    let n = node(dir.path(), Some(t.clone()), None);
    let c = client(&n, &t);
    for i in 0..5u8 {
        let data = bytes(70_000, i);
        let off = c.append(1, &data).unwrap();
        assert_eq!(c.read_exact_at(off, data.len()).unwrap(), data);
    }
    let s = c.rdma_stats().unwrap();
    assert_eq!(s.connects, 1, "{s:?}");
    assert_eq!(s.requests, 10, "{s:?}");
    assert_eq!(
        (
            s.connect_failures,
            s.tcp_fallback_connects,
            s.midflight_errors
        ),
        (0, 0, 0)
    );
    assert_eq!(t.client_opens.load(Ordering::SeqCst), 1);
    assert_eq!(t.server_opens.load(Ordering::SeqCst), 1);
}

#[test]
fn a_node_that_does_not_offer_rdma_is_used_over_tcp_and_not_retried() {
    if std::env::var("ATLAS_NATIVE_RDMA").is_ok_and(|v| v == "1") {
        return; // the host opted its nodes into RDMA; this case is about the default
    }
    let dir = tempfile::tempdir().unwrap();
    let t = FakeTransport::new();
    let n = node(dir.path(), None, None);
    let c = client(&n, &t);
    let data = bytes(10_000, 3);
    let off = c.append(1, &data).unwrap();
    assert_eq!(c.read_exact_at(off, data.len()).unwrap(), data);
    let s = c.rdma_stats().unwrap();
    assert_eq!((s.connects, s.requests), (0, 0), "{s:?}");
    assert_eq!(
        (s.connect_failures, s.tcp_fallback_connects),
        (1, 1),
        "{s:?}"
    );
    assert_eq!(t.client_opens.load(Ordering::SeqCst), 0);
}

#[test]
fn a_transport_that_cannot_connect_falls_back_then_recovers_after_the_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let t = FakeTransport::new();
    let n = node(dir.path(), Some(t.clone()), None);
    let mut cfg = RdmaConfig::new(t.clone(), 1);
    cfg.retry_after = Duration::from_millis(150);
    let c = RemoteDevice::new(n.local_addr(), TIMEOUT)
        .use_rdma(cfg)
        .unwrap();

    t.fail_connect.store(true, Ordering::SeqCst);
    let data = bytes(5_000, 9);
    let off = c.append(1, &data).unwrap();
    assert_eq!(c.read_exact_at(off, data.len()).unwrap(), data);
    let s = c.rdma_stats().unwrap();
    assert_eq!(
        (s.connect_failures, s.tcp_fallback_connects, s.requests),
        (1, 1, 0),
        "{s:?}"
    );

    // After the back-off a new connection tries RDMA again.
    t.fail_connect.store(false, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(250));
    c.close_idle();
    c.len().unwrap();
    let s = c.rdma_stats().unwrap();
    assert!(
        s.connects >= 1 && s.requests >= 1,
        "RDMA should resume: {s:?}"
    );
}

#[test]
fn a_connection_that_breaks_mid_flight_is_retried_over_tcp() {
    let dir = tempfile::tempdir().unwrap();
    let t = FakeTransport::new();
    let n = node(dir.path(), Some(t.clone()), None);
    let c = client(&n, &t);
    let data = bytes(40_000, 5);
    let off = c.append(1, &data).unwrap();
    assert_eq!(c.rdma_stats().unwrap().requests, 1);

    t.break_streams.store(true, Ordering::SeqCst);
    // The pooled RDMA connection now fails; the read succeeds over a fresh TCP connection.
    assert_eq!(c.read_exact_at(off, data.len()).unwrap(), data);
    let s = c.rdma_stats().unwrap();
    assert_eq!(s.midflight_errors, 1, "{s:?}");
    assert!(s.tcp_fallback_connects >= 1, "{s:?}");
    // And RDMA stays paused, so later requests do not hit the broken transport again.
    let off2 = c.append(1, &data).unwrap();
    assert_eq!(c.read_exact_at(off2, data.len()).unwrap(), data);
    assert_eq!(c.rdma_stats().unwrap().midflight_errors, 1);
}

#[test]
fn rdma_writes_work_against_a_node_that_requires_grants() {
    let dir = tempfile::tempdir().unwrap();
    let key = GrantKey::new(b"0123456789abcdef0123").unwrap();
    let t = FakeTransport::new();
    let n = node(dir.path(), Some(t.clone()), Some(key.clone()));
    // A direct-write client holds no key: it presents grants the leader minted for it.
    let c = client(&n, &t);
    let data = bytes(8192, 1);
    let exp = now_ms() + 30_000;

    // Without a grant the node refuses, over RDMA as over TCP.
    let err = c.write_at_granted(None, 1, 0, &data).unwrap_err();
    assert!(
        matches!(err, NativeError::Remote(ref m) if m.contains("grant")),
        "{err:?}"
    );

    // Appends return the node's receipt for the grant.
    let ga = key.mint("g1", "n1", 1, 0, 0, None, data.len() as u64, exp);
    let (off, receipt) = c.append_granted(Some(ga), 1, &data).unwrap();
    assert_eq!(off, 0);
    assert!(receipt.is_some());
    assert_eq!(c.read_exact_at(off, data.len()).unwrap(), data);

    // An in-place write needs a grant for exactly that range.
    let newer = bytes(8192, 2);
    let g = key.mint("g2", "n1", 1, 0, 0, Some(0), newer.len() as u64, exp);
    c.write_at_granted(Some(g), 1, 0, &newer).unwrap();
    assert_eq!(c.read_exact_at(0, newer.len()).unwrap(), newer);

    // A grant for other bytes does not authorize this write.
    let wrong = key.mint("g3", "n1", 1, 0, 0, Some(4096), newer.len() as u64, exp);
    assert!(c.write_at_granted(Some(wrong), 1, 0, &newer).is_err());

    let s = c.rdma_stats().unwrap();
    assert!(s.requests >= 5 && s.tcp_fallback_connects == 0, "{s:?}");
}

/// The live-connection cap: once reached, a new connection uses TCP without counting a failure or
/// backing RDMA off, and the count returns to zero when the connections are dropped.
#[test]
fn reaching_the_connection_cap_uses_tcp_without_a_failure_or_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let t = FakeTransport::new();
    let n = node(dir.path(), Some(t.clone()), None);
    let cfg = RdmaConfig::new(t.clone(), 1).with_max_connections(1);
    assert_eq!(cfg.max_connections, 1);
    let stats = cfg.stats.clone();
    // Two devices sharing one config (as a mount's clients do) share the cap.
    let a = RemoteDevice::new(n.local_addr(), TIMEOUT)
        .use_rdma(cfg.clone())
        .unwrap();
    let b = RemoteDevice::new(n.local_addr(), TIMEOUT)
        .use_rdma(cfg)
        .unwrap();

    let data = bytes(30_000, 2);
    let off = a.append(1, &data).unwrap();
    assert_eq!(stats.snapshot().live_connections, 1);

    // b finds the cap reached and no idle connection: it is served over TCP, twice, correctly.
    assert_eq!(b.read_exact_at(off, data.len()).unwrap(), data);
    assert_eq!(b.read_exact_at(off, data.len()).unwrap(), data);
    let s = stats.snapshot();
    assert_eq!(
        s.capped_fallbacks, 1,
        "the TCP connection is pooled and reused: {s:?}"
    );
    assert_eq!(
        (
            s.connect_failures,
            s.tcp_fallback_connects,
            s.midflight_errors
        ),
        (0, 0, 0),
        "{s:?}"
    );
    assert_eq!(
        (s.connects, s.requests, s.live_connections),
        (1, 1, 1),
        "{s:?}"
    );
    assert_eq!(t.client_opens.load(Ordering::SeqCst), 1);

    // Free the slot: b is not backed off, so its next new connection goes over RDMA.
    a.close_idle();
    assert_eq!(stats.snapshot().live_connections, 0);
    b.close_idle();
    assert_eq!(b.read_exact_at(off, data.len()).unwrap(), data);
    let s = stats.snapshot();
    assert_eq!(
        (s.connects, s.requests, s.live_connections),
        (2, 2, 1),
        "{s:?}"
    );
    assert_eq!((s.capped_fallbacks, s.connect_failures), (1, 0), "{s:?}");

    drop((a, b));
    assert_eq!(stats.snapshot().live_connections, 0);
}

/// A failed setup frees its slot, and `ENOMEM` is counted and explained once.
#[test]
fn enomem_is_counted_explained_once_and_frees_the_slot() {
    let dir = tempfile::tempdir().unwrap();
    let t = FakeTransport::new();
    let n = node(dir.path(), Some(t.clone()), None);
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    let mut cfg = RdmaConfig::new(t.clone(), 1)
        .with_max_connections(1)
        .with_diagnostic_sink(move |m| sink.lock().unwrap().push(m.to_string()));
    cfg.retry_after = Duration::from_millis(50);
    let stats = cfg.stats.clone();
    let c = RemoteDevice::new(n.local_addr(), TIMEOUT)
        .use_rdma(cfg)
        .unwrap();

    t.fail_connect_enomem.store(true, Ordering::SeqCst);
    let off = c.append(1, b"hello").unwrap();
    std::thread::sleep(Duration::from_millis(120));
    c.close_idle();
    assert_eq!(c.read_exact_at(off, 5).unwrap(), b"hello");
    let s = stats.snapshot();
    assert_eq!(
        (s.connect_failures, s.enomem_failures, s.live_connections),
        (2, 2, 0),
        "{s:?}"
    );
    assert_eq!(
        s.capped_fallbacks, 0,
        "a failed setup is not the cap: {s:?}"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        seen[0].contains("RLIMIT_MEMLOCK") && seen[0].contains("LimitMEMLOCK"),
        "{seen:?}"
    );
}

/// Pooled client over real verbs. SoftRoCE (`rxe0` on loopback) is enough; this is not a
/// throughput or latency measurement. Ignored so a host without an RDMA device stays green.
#[cfg(all(feature = "rdma", target_os = "linux"))]
#[test]
#[ignore = "needs an RDMA device; SoftRoCE is enough and hardware is not required (`rdma link add rxe0 type rxe netdev lo`, GID 1 or ATLAS_RDMA_GID_INDEX). Not CI. A pass on rxe0 is plumbing only, not NIC throughput. cargo test -p atlas-native --features rdma -- --ignored pooled_client_over_verbs"]
fn pooled_client_over_verbs() {
    let names = atlas_native::rdma::device_names().expect("list RDMA devices");
    assert!(
        !names.is_empty(),
        "no RDMA device visible to libibverbs (SoftRoCE: rdma link add rxe0 type rxe netdev lo)"
    );
    let gid: u32 = std::env::var("ATLAS_RDMA_GID_INDEX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path(), None, None);
    n.set_rdma_transport(atlas_native::rdma_link::verbs_transport().expect("verbs transport"));
    let mut cfg = RdmaConfig::new(atlas_native::rdma_link::verbs_transport().unwrap(), gid);
    cfg.retry_after = Duration::from_secs(60);
    let c = RemoteDevice::new(n.local_addr(), TIMEOUT)
        .use_rdma(cfg)
        .unwrap();
    let data = bytes(70_000, 4);
    let off = c.append(1, &data).unwrap();
    assert_eq!(c.read_exact_at(off, data.len()).unwrap(), data);
    let s = c.rdma_stats().unwrap();
    assert!(
        s.connects >= 1 && s.requests >= 2,
        "pooled verbs path did not run: {s:?}"
    );
}
