// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeMap,
    net::{SocketAddr, TcpListener},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use atlas_native::{
    BlockStore, DataNodeServer, EngineConfig, FailureDomain, MetaBackend, MetaCommand,
    NativeEngine, NativeError, Node, RaftConfig, RaftError, RaftServer, RemoteDevice, Role,
    TlsIdentity,
};
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair};

const TICK: Duration = Duration::from_millis(10);
const WAIT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(2);

struct Pki {
    ca_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl Pki {
    fn new() -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = params.self_signed(&key).unwrap();
        Self {
            ca_pem: ca.pem(),
            issuer: Issuer::new(params, key),
        }
    }

    /// A node identity whose certificate (DNS SAN `name`) is signed by this CA, trusting
    /// `trust`'s CA.
    fn issue_trusting(&self, name: &str, trust: &Pki) -> TlsIdentity {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![name.to_string()]).unwrap();
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        TlsIdentity::from_pem(
            trust.ca_pem.as_bytes(),
            cert.pem().as_bytes(),
            key.serialize_pem().as_bytes(),
        )
        .unwrap()
    }

    fn issue(&self, name: &str) -> TlsIdentity {
        self.issue_trusting(name, self)
    }
}

fn bind() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}

fn counter(metrics: &str, prefix: &str) -> u64 {
    metrics
        .lines()
        .find_map(|l| l.strip_prefix(prefix))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

fn create(name: &str) -> MetaCommand {
    MetaCommand::CreateVolume {
        id: format!("vol-{name}"),
        name: name.into(),
        size_bytes: 4096,
    }
}

/// Starts a 3-node Raft group where node `id` uses `identities[id]`.
fn raft_group(
    td: &std::path::Path,
    identities: BTreeMap<&str, TlsIdentity>,
) -> BTreeMap<String, RaftServer> {
    let listeners: BTreeMap<String, TcpListener> = identities
        .keys()
        .map(|id| (id.to_string(), bind()))
        .collect();
    let addrs: BTreeMap<String, SocketAddr> = listeners
        .iter()
        .map(|(id, l)| (id.clone(), l.local_addr().unwrap()))
        .collect();
    let mut identities = identities;
    listeners
        .into_iter()
        .map(|(id, l)| {
            let peers: BTreeMap<String, SocketAddr> = addrs
                .iter()
                .filter(|(p, _)| **p != id)
                .map(|(p, a)| (p.clone(), *a))
                .collect();
            let cfg = RaftConfig::new(id.clone(), peers.keys().cloned().collect(), td.join(&id));
            let tls = identities.remove(id.as_str()).unwrap();
            let s = RaftServer::start_with(cfg, l, peers, TICK, Some(tls)).unwrap();
            (id, s)
        })
        .collect()
}

fn propose_until_ok(servers: &BTreeMap<String, RaftServer>, cmd: MetaCommand) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
        for (id, s) in servers {
            match s.propose(cmd.clone(), Duration::from_secs(2)) {
                Ok(_) => return id.clone(),
                Err(
                    RaftError::NotLeader { .. }
                    | RaftError::LeadershipLost { .. }
                    | RaftError::Timeout { .. },
                ) => {}
                Err(e) => panic!("{id}: {e}"),
            }
        }
        assert!(Instant::now() < deadline, "no leader accepted the proposal");
        thread::sleep(TICK);
    }
}

fn wait_has_volume(s: &RaftServer, name: &str) {
    let deadline = Instant::now() + WAIT;
    while !s
        .catalog()
        .unwrap()
        .volumes
        .contains_key(&format!("vol-{name}"))
    {
        assert!(Instant::now() < deadline, "volume {name} never replicated");
        thread::sleep(TICK);
    }
}

#[test]
fn mtls_raft_cluster_elects_and_replicates() {
    let td = tempfile::tempdir().unwrap();
    let pki = Pki::new();
    let ids = BTreeMap::from([
        ("m1", pki.issue("m1")),
        ("m2", pki.issue("m2")),
        ("m3", pki.issue("m3")),
    ]);
    let servers = raft_group(td.path(), ids);
    propose_until_ok(&servers, create("a"));
    for s in servers.values() {
        wait_has_volume(s, "a");
    }
    for (id, s) in &servers {
        let m = s.render_metrics().unwrap();
        let prefix = format!(
            "atlas_native_transport_tls_handshake_failures_total{{node=\"{id}\",group=\"0\"}} "
        );
        assert_eq!(counter(&m, &prefix), 0, "{id}:\n{m}");
    }
}

#[test]
fn mtls_raft_isolates_untrusted_and_impersonating_peers() {
    let td = tempfile::tempdir().unwrap();
    let pki = Pki::new();
    let rogue = Pki::new();
    // m3 holds a CA-signed certificate, but for the name "m2": it can neither be dialled as m3
    // nor speak as m3.
    let ids = BTreeMap::from([
        ("m1", pki.issue("m1")),
        ("m2", pki.issue("m2")),
        ("m3", pki.issue("m2")),
    ]);
    let servers = raft_group(td.path(), ids);
    let leader = propose_until_ok(&servers, create("a"));
    assert_ne!(leader, "m3");
    wait_has_volume(&servers["m1"], "a");
    wait_has_volume(&servers["m2"], "a");
    thread::sleep(Duration::from_millis(300));
    assert!(servers["m3"].catalog().unwrap().volumes.is_empty());
    assert_ne!(servers["m3"].status().unwrap().role, Role::Leader);
    let rejected: u64 = ["m1", "m2"]
        .iter()
        .map(|id| {
            counter(
                &servers[*id].render_metrics().unwrap(),
                &format!(
                    "atlas_native_transport_rejected_frames_total{{node=\"{id}\",group=\"0\"}} "
                ),
            )
        })
        .sum();
    assert!(rejected > 0, "frames claiming to be from m3 were accepted");
    drop(servers);

    // A peer whose certificate comes from another CA never completes a handshake.
    let td = tempfile::tempdir().unwrap();
    let ids = BTreeMap::from([
        ("m1", pki.issue("m1")),
        ("m2", pki.issue("m2")),
        ("m3", rogue.issue_trusting("m3", &pki)),
    ]);
    let servers = raft_group(td.path(), ids);
    propose_until_ok(&servers, create("b"));
    wait_has_volume(&servers["m1"], "b");
    wait_has_volume(&servers["m2"], "b");
    thread::sleep(Duration::from_millis(300));
    assert!(servers["m3"].catalog().unwrap().volumes.is_empty());
    let failures = counter(
        &servers["m1"].render_metrics().unwrap(),
        "atlas_native_transport_tls_handshake_failures_total{node=\"m1\",group=\"0\"} ",
    );
    assert!(failures > 0, "m1 accepted a rogue-CA peer");
}

#[test]
fn mtls_data_node_authenticates_both_sides() {
    let td = tempfile::tempdir().unwrap();
    let pki = Pki::new();
    let rogue = Pki::new();
    let dn = DataNodeServer::start_with("n1", td.path(), bind(), Some(pki.issue("n1"))).unwrap();
    let addr = dn.local_addr();

    let client = pki.issue("engine");
    let dev = RemoteDevice::with_tls(addr, "n1", &client, IO_TIMEOUT).unwrap();
    assert_eq!(dev.append(1, b"secret").unwrap(), 0);
    assert_eq!(dev.read_exact_at(0, 6).unwrap(), b"secret");

    // Plaintext, a node-id mismatch, and a rogue-CA client are all refused.
    let plain = RemoteDevice::new(addr, IO_TIMEOUT);
    assert!(plain.len().is_err());
    let wrong_name = RemoteDevice::with_tls(addr, "n2", &client, IO_TIMEOUT).unwrap();
    assert!(matches!(wrong_name.len(), Err(NativeError::Io(_))));
    let intruder = rogue.issue_trusting("engine", &pki);
    let rogue_dev = RemoteDevice::with_tls(addr, "n1", &intruder, IO_TIMEOUT).unwrap();
    assert!(rogue_dev.append(1, b"evil").is_err());

    assert_eq!(dev.read_exact_at(0, 6).unwrap(), b"secret");
    assert_eq!(dev.len().unwrap(), 6);
    let m = dn.render_metrics().unwrap();
    assert!(
        counter(
            &m,
            "atlas_native_data_tls_handshake_failures_total{node=\"n1\"} "
        ) >= 2,
        "{m}"
    );
}

#[test]
fn mtls_engine_writes_through_raft_to_tls_data_nodes() {
    let td = tempfile::tempdir().unwrap();
    let pki = Pki::new();
    let dns: Vec<DataNodeServer> = (1..=3)
        .map(|i| {
            let id = format!("n{i}");
            let identity = pki.issue(&id);
            DataNodeServer::start_with(&id, td.path().join(&id), bind(), Some(identity)).unwrap()
        })
        .collect();
    let ids = BTreeMap::from([
        ("m1", pki.issue("m1")),
        ("m2", pki.issue("m2")),
        ("m3", pki.issue("m3")),
    ]);
    let servers = raft_group(&td.path().join("raft"), ids);
    let engines: Vec<NativeEngine> = servers
        .into_iter()
        .map(|(id, s)| {
            let client = pki.issue(&id);
            let stores = dns
                .iter()
                .enumerate()
                .map(|(i, dn)| {
                    let node_id = format!("n{}", i + 1);
                    let dev =
                        RemoteDevice::with_tls(dn.local_addr(), &node_id, &client, IO_TIMEOUT)
                            .unwrap();
                    let node = Node {
                        id: node_id,
                        failure_domain: FailureDomain {
                            zone: "z1".into(),
                            rack: format!("r{i}"),
                            host: format!("h{i}"),
                        },
                        free_bytes: 1 << 30,
                        healthy: true,
                    };
                    (node, Arc::new(dev) as Arc<dyn BlockStore>)
                })
                .collect();
            let mut cfg = EngineConfig::new(td.path().join(&id).join("engine"));
            cfg.extent_bytes = 4096;
            NativeEngine::open_with(
                cfg,
                stores,
                MetaBackend::Raft {
                    server: Arc::new(s),
                    timeout: Duration::from_secs(5),
                },
            )
            .unwrap()
        })
        .collect();

    let deadline = Instant::now() + WAIT;
    let (v, leader) = loop {
        let mut done = None;
        for (i, e) in engines.iter().enumerate() {
            let attempt = e
                .create_volume("v", 4096)
                .and_then(|v| e.write(&v, 0, &[5u8; 4096]).map(|()| v));
            match attempt {
                Ok(v) => {
                    done = Some((v, i));
                    break;
                }
                Err(NativeError::Raft(_)) => {}
                Err(other) => panic!("engine {i}: {other}"),
            }
        }
        if let Some(d) = done {
            break d;
        }
        assert!(Instant::now() < deadline, "no engine accepted the write");
        thread::sleep(TICK);
    };
    for e in &engines {
        let deadline = Instant::now() + WAIT;
        while e.read(&v, 0, 4096).ok().as_deref() != Some(&[5u8; 4096][..]) {
            assert!(Instant::now() < deadline, "replica never saw the write");
            thread::sleep(TICK);
        }
    }
    assert!(engines[leader].device_len("n1").unwrap() >= 4096);
}

/// RDMA carries the data-node protocol in plaintext, so asking for it on a TLS client is a
/// configuration error, not a quiet downgrade.
#[test]
fn rdma_is_refused_on_a_tls_client() {
    let pki = Pki::new();
    let id = pki.issue("client");
    let dev = RemoteDevice::with_tls("127.0.0.1:1", "n1", &id, IO_TIMEOUT).unwrap();
    let cfg = atlas_native::RdmaConfig::new(atlas_native::FakeTransport::new(), 1);
    let err = dev.use_rdma(cfg).unwrap_err();
    match err {
        NativeError::Invalid(m) => assert!(m.contains("plaintext") && m.contains("TLS"), "{m}"),
        other => panic!("expected a configuration error, got {other:?}"),
    }
}
