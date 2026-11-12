// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! Mutual TLS for the Raft and data-node transports. Every node holds a certificate signed by a
//! shared cluster CA whose DNS SAN is its node id; both sides of every connection verify the other.
//!
//! Uses rustls with an explicit `ring` provider, so it never depends on (or installs) the
//! process-wide default provider.

use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};

use rustls::{
    client::ClientConnection,
    crypto::CryptoProvider,
    pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer, ServerName},
    server::{ServerConnection, WebPkiClientVerifier},
    ClientConfig, RootCertStore, ServerConfig, StreamOwned,
};

/// A node's TLS identity: the cluster CA bundle, its certificate chain and private key (PEM).
#[derive(Debug)]
pub struct TlsIdentity {
    roots: Arc<RootCertStore>,
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

impl Clone for TlsIdentity {
    fn clone(&self) -> Self {
        Self {
            roots: self.roots.clone(),
            chain: self.chain.clone(),
            key: self.key.clone_key(),
        }
    }
}

fn invalid(msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn roots_from_pem(ca_pem: &[u8]) -> io::Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    for ca in CertificateDer::pem_slice_iter(ca_pem) {
        roots
            .add(ca.map_err(|e| invalid(format!("CA bundle: {e}")))?)
            .map_err(|e| invalid(format!("CA certificate: {e}")))?;
    }
    if roots.is_empty() {
        return Err(invalid("CA bundle has no certificates"));
    }
    Ok(roots)
}

fn chain_and_key(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> io::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let chain = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| invalid(format!("certificate: {e}")))?;
    if chain.is_empty() {
        return Err(invalid("certificate file has no certificates"));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| invalid(format!("key: {e}")))?;
    Ok((chain, key))
}

/// Server config for the HTTPS API. With `client_ca`, client certificates signed by it are
/// verified when presented but not required; the caller decides which requests need one (see
/// [`Conn::client_verified`]).
pub(crate) fn http_server_config(
    cert_pem: &[u8],
    key_pem: &[u8],
    client_ca: Option<&[u8]>,
) -> io::Result<Arc<ServerConfig>> {
    let (chain, key) = chain_and_key(cert_pem, key_pem)?;
    let builder = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(invalid)?;
    let builder = match client_ca {
        Some(ca) => builder.with_client_cert_verifier(
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots_from_pem(ca)?), provider())
                .allow_unauthenticated()
                .build()
                .map_err(invalid)?,
        ),
        None => builder.with_no_client_auth(),
    };
    Ok(Arc::new(
        builder.with_single_cert(chain, key).map_err(invalid)?,
    ))
}

impl TlsIdentity {
    pub fn from_pem(ca_pem: &[u8], cert_pem: &[u8], key_pem: &[u8]) -> io::Result<Self> {
        let roots = roots_from_pem(ca_pem)?;
        let (chain, key) = chain_and_key(cert_pem, key_pem)?;
        Ok(Self {
            roots: Arc::new(roots),
            chain,
            key,
        })
    }

    pub fn from_pem_files(
        ca: impl AsRef<std::path::Path>,
        cert: impl AsRef<std::path::Path>,
        key: impl AsRef<std::path::Path>,
    ) -> io::Result<Self> {
        Self::from_pem(
            &std::fs::read(ca)?,
            &std::fs::read(cert)?,
            &std::fs::read(key)?,
        )
    }

    /// Server side: requires a client certificate signed by the cluster CA.
    pub(crate) fn server_config(&self) -> io::Result<Arc<ServerConfig>> {
        self.server_config_for(true)
    }

    /// Server side. `require_client_cert` is the historical mutual-TLS rule. When it is false a
    /// missing client certificate still completes the handshake (so a bearer can authenticate
    /// the session) and a presented certificate is verified against the cluster CA.
    pub(crate) fn server_config_for(&self, require_client_cert: bool) -> io::Result<Arc<ServerConfig>> {
        let mut builder =
            WebPkiClientVerifier::builder_with_provider(self.roots.clone(), provider());
        if !require_client_cert {
            builder = builder.allow_unauthenticated();
        }
        let verifier = builder.build().map_err(invalid)?;
        let cfg = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(invalid)?
            .with_client_cert_verifier(verifier)
            .with_single_cert(self.chain.clone(), self.key.clone_key())
            .map_err(invalid)?;
        Ok(Arc::new(cfg))
    }

    /// Client side: presents this identity and verifies the server against the cluster CA.
    pub(crate) fn client_config(&self) -> io::Result<Arc<ClientConfig>> {
        let cfg = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(invalid)?
            .with_root_certificates(self.roots.clone())
            .with_client_auth_cert(self.chain.clone(), self.key.clone_key())
            .map_err(invalid)?;
        Ok(Arc::new(cfg))
    }
}

/// Resolves `host:port` (or a literal socket address) now. Callers resolve on every connect so a
/// peer whose IP changes (e.g. a rescheduled pod behind a stable DNS name) is found again.
pub(crate) fn resolve(target: &str) -> io::Result<SocketAddr> {
    target.to_socket_addrs()?.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{target} resolved to no addresses"),
        )
    })
}

/// `name` as a TLS server name; node ids must be valid DNS names to be used with TLS.
pub(crate) fn server_name(name: &str) -> io::Result<ServerName<'static>> {
    ServerName::try_from(name.to_string()).map_err(|e| invalid(format!("server name {name}: {e}")))
}

/// A plaintext or TLS connection over a blocking `TcpStream`.
pub(crate) enum Conn {
    Plain(TcpStream),
    Server(Box<StreamOwned<ServerConnection, TcpStream>>),
    Client(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Conn {
    /// Wraps an accepted socket, completing the TLS handshake (bounded by `timeout`) when
    /// `tls` is set.
    pub(crate) fn accept(
        sock: TcpStream,
        tls: Option<&Arc<ServerConfig>>,
        timeout: Duration,
    ) -> io::Result<Self> {
        let Some(cfg) = tls else {
            return Ok(Self::Plain(sock));
        };
        let prior = sock.read_timeout()?;
        sock.set_read_timeout(Some(timeout))?;
        let conn = ServerConnection::new(cfg.clone()).map_err(io::Error::other)?;
        let mut s = StreamOwned::new(conn, sock);
        while s.conn.is_handshaking() {
            s.conn.complete_io(&mut s.sock)?;
        }
        s.sock.set_read_timeout(prior)?;
        Ok(Self::Server(Box::new(s)))
    }

    /// Wraps a connected socket (whose timeouts bound the handshake), verifying the server as
    /// `name` when `tls` is set.
    pub(crate) fn connect(
        sock: TcpStream,
        tls: Option<(&Arc<ClientConfig>, ServerName<'static>)>,
    ) -> io::Result<Self> {
        let Some((cfg, name)) = tls else {
            return Ok(Self::Plain(sock));
        };
        let conn = ClientConnection::new(cfg.clone(), name).map_err(io::Error::other)?;
        let mut s = StreamOwned::new(conn, sock);
        while s.conn.is_handshaking() {
            s.conn.complete_io(&mut s.sock)?;
        }
        Ok(Self::Client(Box::new(s)))
    }

    /// Of `candidates`, the names the verified client certificate is valid for. Empty for a
    /// plaintext connection.
    pub(crate) fn peer_names<'a>(&self, candidates: &'a [String]) -> Vec<&'a String> {
        let Self::Server(s) = self else {
            return Vec::new();
        };
        let Some(cert) = s.conn.peer_certificates().and_then(|c| c.first()) else {
            return Vec::new();
        };
        let Ok(ee) = webpki::EndEntityCert::try_from(cert) else {
            return Vec::new();
        };
        candidates
            .iter()
            .filter(|c| {
                server_name(c)
                    .map(|n| ee.verify_is_valid_for_subject_name(&n).is_ok())
                    .unwrap_or(false)
            })
            .collect()
    }

    pub(crate) fn is_tls(&self) -> bool {
        !matches!(self, Self::Plain(_))
    }

    /// Whether the peer presented a certificate that the server's client verifier accepted
    /// (rustls aborts the handshake on an invalid one).
    pub(crate) fn client_verified(&self) -> bool {
        matches!(self, Self::Server(s) if s.conn.peer_certificates().is_some_and(|c| !c.is_empty()))
    }

    /// Sends TLS close_notify (best effort) so the peer sees a clean end of stream.
    pub(crate) fn close_notify(&mut self) {
        if let Self::Server(s) = self {
            s.conn.send_close_notify();
            while s.conn.wants_write() {
                if s.conn.write_tls(&mut s.sock).is_err() {
                    break;
                }
            }
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::Server(s) => s.read(buf),
            Self::Client(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            Self::Server(s) => s.write(buf),
            Self::Client(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Server(s) => s.flush(),
            Self::Client(s) => s.flush(),
        }
    }
}

/// PEM paths a data node re-reads when they change or when [`request_cert_reload`] is set.
#[derive(Clone, Debug)]
pub struct TlsPaths {
    pub ca: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
}

static CERT_RELOAD: AtomicBool = AtomicBool::new(false);

/// Asks every data node in this process to reload TLS from its configured paths. The SIGHUP
/// handler and tests both call this; the accept loop applies it.
pub fn request_cert_reload() {
    CERT_RELOAD.store(true, Ordering::Relaxed);
}

pub(crate) fn take_cert_reload() -> bool {
    CERT_RELOAD.swap(false, Ordering::Relaxed)
}

/// Installs a SIGHUP handler that sets the reload flag. The data-node accept loop (and a periodic
/// mtime check) performs the reload. Unix only.
pub fn install_cert_reload_sighup() {
    #[cfg(unix)]
    unsafe {
        libc::signal(
            libc::SIGHUP,
            on_sighup as *const () as libc::sighandler_t,
        );
    }
}

#[cfg(unix)]
extern "C" fn on_sighup(_: libc::c_int) {
    CERT_RELOAD.store(true, Ordering::Relaxed);
}

/// Watches [`TlsPaths`] and rebuilds the data-node server config when the files change.
pub(crate) struct TlsWatch {
    paths: TlsPaths,
    /// When false, a missing client certificate is allowed at the handshake (bearer auth).
    require_client_cert: bool,
    mtimes: [Option<SystemTime>; 3],
    generation: u64,
}

impl TlsWatch {
    pub(crate) fn new(paths: TlsPaths, require_client_cert: bool) -> io::Result<Self> {
        let mtimes = stat_paths(&paths)?;
        Ok(Self {
            paths,
            require_client_cert,
            mtimes,
            generation: 0,
        })
    }

    pub(crate) fn stale(&self) -> bool {
        match stat_paths(&self.paths) {
            Ok(now) => now != self.mtimes,
            Err(_) => true,
        }
    }

    pub(crate) fn reload(&mut self) -> io::Result<Arc<ServerConfig>> {
        let id = TlsIdentity::from_pem_files(&self.paths.ca, &self.paths.cert, &self.paths.key)?;
        let cfg = id.server_config_for(self.require_client_cert)?;
        self.mtimes = stat_paths(&self.paths)?;
        self.generation = self.generation.saturating_add(1);
        Ok(cfg)
    }
}

fn stat_paths(paths: &TlsPaths) -> io::Result<[Option<SystemTime>; 3]> {
    let mut out = [None, None, None];
    for (i, p) in [&paths.ca, &paths.cert, &paths.key].into_iter().enumerate() {
        out[i] = Some(std::fs::metadata(p)?.modified()?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_flag_is_taken_once() {
        request_cert_reload();
        assert!(take_cert_reload());
        assert!(!take_cert_reload());
    }

    #[test]
    fn replaced_certificate_is_a_new_generation() {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};

        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| {
            std::fs::write(dir.path().join(name), body).unwrap();
        };
        let issue = || {
            let key = KeyPair::generate().unwrap();
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            let ca = params.self_signed(&key).unwrap();
            let issuer = Issuer::new(params, key);
            let leaf_key = KeyPair::generate().unwrap();
            let leaf_params = CertificateParams::new(vec!["n1".to_string()]).unwrap();
            let leaf = leaf_params.signed_by(&leaf_key, &issuer).unwrap();
            (ca.pem(), leaf.pem(), leaf_key.serialize_pem())
        };
        let (ca, cert, key) = issue();
        write("ca.pem", &ca);
        write("cert.pem", &cert);
        write("key.pem", &key);
        let paths = TlsPaths {
            ca: dir.path().join("ca.pem"),
            cert: dir.path().join("cert.pem"),
            key: dir.path().join("key.pem"),
        };
        let mut watch = TlsWatch::new(paths, true).unwrap();
        assert!(!watch.stale());
        assert_eq!(watch.generation, 0);
        let (ca, cert, key) = issue();
        write("ca.pem", &ca);
        write("cert.pem", &cert);
        write("key.pem", &key);
        // `reload` is what SIGHUP calls even when the filesystem timestamp did not move.
        watch.reload().unwrap();
        assert_eq!(watch.generation, 1);
        watch.reload().unwrap();
        assert_eq!(watch.generation, 2);
    }
}
