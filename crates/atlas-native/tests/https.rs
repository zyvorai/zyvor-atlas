// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use atlas_native::node::{
    DataNodeRole, DataNodeSpec, HttpTlsFiles, Listeners, MetadataRole, NativeNode, NodeConfig,
};
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair};
use rustls::{
    pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer, ServerName},
    ClientConfig, ClientConnection, RootCertStore, StreamOwned,
};

const TOKEN: &str = "https-token";

struct Ca {
    pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl Ca {
    fn new() -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = params.self_signed(&key).unwrap();
        Self {
            pem: ca.pem(),
            issuer: Issuer::new(params, key),
        }
    }

    /// (certificate PEM, key PEM) for `name`.
    fn issue(&self, name: &str, usage: ExtendedKeyUsagePurpose) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![name.to_string()]).unwrap();
        params.extended_key_usages = vec![usage];
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        (cert.pem(), key.serialize_pem())
    }
}

fn client_config(server_ca: &str, client: Option<&(String, String)>) -> Arc<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(server_ca.as_bytes()) {
        roots.add(c.unwrap()).unwrap();
    }
    let b = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots);
    Arc::new(match client {
        Some((cert, key)) => b
            .with_client_auth_cert(
                CertificateDer::pem_slice_iter(cert.as_bytes())
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
                PrivateKeyDer::from_pem_slice(key.as_bytes()).unwrap(),
            )
            .unwrap(),
        None => b.with_no_client_auth(),
    })
}

/// One HTTPS request; `Err` if the TLS handshake or exchange fails.
fn https(
    addr: SocketAddr,
    cfg: &Arc<ClientConfig>,
    method: &str,
    path: &str,
    body: &[u8],
) -> std::io::Result<(u16, Vec<u8>)> {
    let sock = TcpStream::connect(addr)?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    let conn = ClientConnection::new(cfg.clone(), ServerName::try_from("localhost").unwrap())
        .map_err(std::io::Error::other)?;
    let mut s = StreamOwned::new(conn, sock);
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    let mut out = Vec::new();
    s.read_to_end(&mut out)?;
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("no response head"))?;
    let status = std::str::from_utf8(&out[9..12]).unwrap().parse().unwrap();
    Ok((status, out[split + 4..].to_vec()))
}

fn start_node(root: &Path, server: &(String, String), client_ca: &str) -> NativeNode {
    std::fs::write(root.join("token"), TOKEN).unwrap();
    std::fs::write(root.join("http.pem"), &server.0).unwrap();
    std::fs::write(root.join("http-key.pem"), &server.1).unwrap();
    std::fs::write(root.join("client-ca.pem"), client_ca).unwrap();
    let data_l = TcpListener::bind("127.0.0.1:0").unwrap();
    let raft_l = TcpListener::bind("127.0.0.1:0").unwrap();
    let cfg = NodeConfig {
        node_id: "n1".into(),
        data_dir: root.join("n1"),
        http_listen: "127.0.0.1:0".parse().unwrap(),
        api_token_file: Some(root.join("token")),
        tls: None,
        http_tls: Some(HttpTlsFiles {
            cert: root.join("http.pem"),
            key: root.join("http-key.pem"),
            client_ca: Some(root.join("client-ca.pem")),
        }),
        data_node: Some(DataNodeRole {
            listen: data_l.local_addr().unwrap(),
            devices: Vec::new(),
        }),
        metadata: Some(MetadataRole {
            listen: raft_l.local_addr().unwrap(),
            peers: BTreeMap::from([("n1".into(), raft_l.local_addr().unwrap().to_string())]),
            bootstrap: None,
            data_nodes: vec![DataNodeSpec {
                id: "n1".into(),
                addr: data_l.local_addr().unwrap().to_string(),
                zone: None,
                rack: None,
                host: None,
                free_bytes: 1 << 30,
                devices: 1,
            }],
            replicas: 1,
            erasure: None,
            erasure_min_bytes: 64 << 10,
            compression: false,
            rebuild_delay_secs: 60,
            rebuild_bytes_per_sec: 0,
            scrub_bytes_per_sec: 0,
            tiering: None,
            extent_bytes: 4096,
            tick_ms: 10,
            proposal_timeout_ms: 3000,
            repair_interval_secs: 0,
            gc_interval_secs: 0,
            groups: 1,
            cache_inodes: atlas_native::store::DEFAULT_CACHE_INODES,
            store_cache_bytes: atlas_native::store::DEFAULT_STORE_CACHE_BYTES,
        }),
        max_request_bytes: 1 << 20,
    };
    NativeNode::start_with(
        cfg,
        Listeners {
            http: Some(TcpListener::bind("127.0.0.1:0").unwrap()),
            data_node: Some(data_l),
            metadata: Some(raft_l),
        },
    )
    .unwrap()
}

#[test]
fn https_api_requires_client_certificate_for_v1_only() {
    let td = tempfile::tempdir().unwrap();
    let server_ca = Ca::new();
    let client_ca = Ca::new();
    let rogue_ca = Ca::new();
    let server = server_ca.issue("localhost", ExtendedKeyUsagePurpose::ServerAuth);
    let node = start_node(td.path(), &server, &client_ca.pem);
    let addr = node.http_addr();

    let anonymous = client_config(&server_ca.pem, None);
    let trusted = client_config(
        &server_ca.pem,
        Some(&client_ca.issue("ops", ExtendedKeyUsagePurpose::ClientAuth)),
    );
    let rogue = client_config(
        &server_ca.pem,
        Some(&rogue_ca.issue("ops", ExtendedKeyUsagePurpose::ClientAuth)),
    );

    // Probes work over TLS without a client certificate.
    let deadline = Instant::now() + Duration::from_secs(20);
    while https(addr, &anonymous, "GET", "/readyz", b"")
        .map(|r| r.0)
        .ok()
        != Some(200)
    {
        assert!(Instant::now() < deadline, "node never became ready");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        https(addr, &anonymous, "GET", "/metrics", b"").unwrap().0,
        200
    );

    // The API does not, even with the right token.
    assert_eq!(
        https(addr, &anonymous, "GET", "/v1/status", b"").unwrap().0,
        401
    );
    // A certificate from another CA fails the handshake.
    assert!(https(addr, &rogue, "GET", "/v1/status", b"").is_err());
    // Plain HTTP to the TLS port gets no HTTP response.
    let mut plain = TcpStream::connect(addr).unwrap();
    plain
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let _ = plain.write_all(b"GET /healthz HTTP/1.1\r\n\r\n");
    let mut buf = Vec::new();
    let _ = plain.read_to_end(&mut buf);
    assert!(!buf.starts_with(b"HTTP/1.1"), "plaintext was answered");

    let (st, b) = https(
        addr,
        &trusted,
        "POST",
        "/v1/volumes",
        br#"{"name":"v","size_bytes":8192}"#,
    )
    .unwrap();
    assert_eq!(st, 201, "{}", String::from_utf8_lossy(&b));
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    let v = v["id"].as_str().unwrap();
    let path = format!("/v1/volumes/{v}/data?offset=100");
    assert_eq!(
        https(addr, &trusted, "PUT", &path, &[3u8; 5000]).unwrap().0,
        204
    );
    let (st, back) = https(
        addr,
        &trusted,
        "GET",
        &format!("/v1/volumes/{v}/data?offset=100&len=5000"),
        b"",
    )
    .unwrap();
    assert_eq!(st, 200);
    assert_eq!(back, vec![3u8; 5000]);
}
