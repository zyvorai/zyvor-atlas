// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `atlas-native-s3 --endpoint https://node:7400 --buckets-file buckets.json --credentials-file
//! credentials.json`: serves atlas-native filesystems over the S3 API (path-style, and
//! virtual-hosted style for `--domain`). Runs until SIGTERM or SIGINT, then finishes in-flight
//! requests and closes its cluster sessions.

use std::{net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};

use atlas_native_fuse::{
    client::{Client, ClientConfig},
    ops::{Ops, OpsConfig},
};
use atlas_native_s3::{
    config::{self, BucketSpec, CredentialSpec},
    service::Gateway,
    store::Bucket,
};
use clap::Parser;
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder as ConnBuilder,
};
use s3s::{auth::SimpleAuth, host::MultiDomain, service::S3ServiceBuilder};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

#[derive(Parser)]
#[command(version, about = "Serve atlas-native filesystems over the S3 API")]
struct Args {
    /// Metadata node base URL; repeat (or comma-separate) for every node.
    #[arg(
        long,
        env = "ATLAS_NATIVE_ENDPOINTS",
        value_delimiter = ',',
        required = true
    )]
    endpoint: Vec<String>,
    /// File holding the cluster API bearer token.
    #[arg(long, env = "ATLAS_NATIVE_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// PEM CA bundle for HTTPS endpoints.
    #[arg(long)]
    ca_file: Option<PathBuf>,
    /// PEM client certificate and key (one file, certificate first) for nodes that require mTLS.
    #[arg(long)]
    identity_file: Option<PathBuf>,
    /// JSON list of buckets: `[{"bucket": "team-share", "fs": "team-share"}]`.
    #[arg(long)]
    buckets_file: PathBuf,
    /// JSON list of access keys: `[{"access_key": "...", "secret_key": "..."}]`.
    #[arg(long)]
    credentials_file: PathBuf,
    #[arg(long, default_value = "0.0.0.0:9000")]
    listen: SocketAddr,
    /// Serve HTTPS with this PEM certificate chain (and `--tls-key`).
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// Domain for virtual-hosted-style requests (`<bucket>.<domain>`); repeatable.
    #[arg(long)]
    domain: Vec<String>,
    /// Owner of the files and directories the gateway creates.
    #[arg(long, default_value_t = 0)]
    uid: u32,
    #[arg(long, default_value_t = 0)]
    gid: u32,
    /// Keep retrying through elections and unreachable nodes for this long before failing a call.
    #[arg(long, default_value_t = 30)]
    retry_secs: u64,
    /// Largest single request to the cluster; keep at or below the nodes' `max_request_bytes`.
    #[arg(long, default_value_t = 8 << 20)]
    max_io_bytes: usize,
}

fn read(path: &PathBuf, what: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{what} {}: {e}", path.display()))
}

fn bucket(args: &Args, spec: &BucketSpec) -> Result<Bucket, String> {
    let mut cfg = ClientConfig::new(args.endpoint.clone());
    cfg.token = args
        .token_file
        .as_ref()
        .map(|p| read(p, "token file").map(|t| String::from_utf8_lossy(&t).trim().to_string()))
        .transpose()?;
    cfg.ca_pem = args
        .ca_file
        .as_ref()
        .map(|p| read(p, "CA file"))
        .transpose()?;
    cfg.identity_pem = args
        .identity_file
        .as_ref()
        .map(|p| read(p, "identity file"))
        .transpose()?;
    cfg.retry_for = Duration::from_secs(args.retry_secs);
    let client = Client::new(cfg).map_err(|e| e.to_string())?;
    let ops = Ops::new(
        client,
        spec.target(),
        OpsConfig {
            max_io_bytes: args.max_io_bytes,
            // Under cache leases nothing this gateway caches is ever stale, whatever other
            // gateways, NFS or FUSE clients change.
            cache_leases: true,
            // Readahead windows are not covered by leases; objects are read in large chunks.
            readahead_bytes: 0,
            lock_waiters: 0,
            ..OpsConfig::default()
        },
    );
    ops.getattr(atlas_native::ROOT_INO).map_err(|e| {
        format!(
            "bucket {}: filesystem {} is not reachable (errno {e})",
            spec.bucket,
            ops.fs()
        )
    })?;
    let b = Bucket {
        name: spec.bucket.clone(),
        ops,
        read_only: spec.read_only,
        uid: args.uid,
        gid: args.gid,
    };
    b.stamp_created()
        .map_err(|e| format!("bucket {}: {e}", spec.bucket))?;
    Ok(b)
}

fn tls(args: &Args) -> Result<Option<TlsAcceptor>, String> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let (Some(cert), Some(key)) = (&args.tls_cert, &args.tls_key) else {
        return Ok(None);
    };
    let certs = CertificateDer::pem_file_iter(cert)
        .and_then(|i| i.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("TLS certificate {}: {e}", cert.display()))?;
    let key =
        PrivateKeyDer::from_pem_file(key).map_err(|e| format!("TLS key {}: {e}", key.display()))?;
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS: {e}"))?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Some(TlsAcceptor::from(Arc::new(cfg))))
}

async fn run(args: Args) -> Result<(), String> {
    let specs: Vec<BucketSpec> = serde_json::from_slice(&read(&args.buckets_file, "buckets file")?)
        .map_err(|e| format!("buckets file: {e}"))?;
    config::validate_buckets(&specs)?;
    // The parse error never quotes the file: it holds secrets.
    let creds: Vec<CredentialSpec> =
        serde_json::from_slice(&read(&args.credentials_file, "credentials file")?).map_err(
            |e| {
                format!(
                    "credentials file is not a valid list of access keys (line {})",
                    e.line()
                )
            },
        )?;
    let grants = config::grants(&creds, &specs)?;
    let mut auth = SimpleAuth::new();
    for c in creds {
        auth.register(c.access_key, c.secret_key.into());
    }
    let buckets = {
        let args = &args;
        let specs = specs.clone();
        tokio::task::block_in_place(|| {
            specs
                .iter()
                .map(|s| bucket(args, s))
                .collect::<Result<Vec<_>, _>>()
        })?
    };
    let gateway = Gateway::new(buckets, Some(grants));
    let service = {
        let mut b = S3ServiceBuilder::new(gateway.clone());
        b.set_auth(auth);
        if !args.domain.is_empty() {
            b.set_host(MultiDomain::new(&args.domain).map_err(|e| format!("--domain: {e}"))?);
        }
        b.build()
    };
    let acceptor = tls(&args)?;
    let listener = TcpListener::bind(args.listen)
        .await
        .map_err(|e| format!("listen {}: {e}", args.listen))?;
    tracing::info!(
        listen = %args.listen,
        tls = acceptor.is_some(),
        buckets = specs.len(),
        "serving S3"
    );
    let http = ConnBuilder::new(TokioExecutor::new());
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| format!("signal handler: {e}"))?;
    loop {
        let (socket, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    continue;
                }
            },
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
        };
        let _ = socket.set_nodelay(true);
        match &acceptor {
            Some(acceptor) => {
                let (acceptor, http, service) = (acceptor.clone(), http.clone(), service.clone());
                let watcher = graceful.watcher();
                tokio::spawn(async move {
                    match acceptor.accept(socket).await {
                        Ok(stream) => {
                            let conn = http
                                .serve_connection(TokioIo::new(stream), service)
                                .into_owned();
                            let _ = watcher.watch(conn).await;
                        }
                        Err(e) => tracing::debug!(%peer, error = %e, "TLS handshake failed"),
                    }
                });
            }
            None => {
                let conn = graceful.watch(
                    http.serve_connection(TokioIo::new(socket), service.clone())
                        .into_owned(),
                );
                tokio::spawn(async move {
                    let _ = conn.await;
                });
            }
        }
    }
    tracing::info!("shutting down");
    tokio::select! {
        () = graceful.shutdown() => {}
        () = tokio::time::sleep(Duration::from_secs(30)) => tracing::warn!("requests still running after 30 s"),
    }
    tokio::task::block_in_place(|| {
        for b in gateway.buckets() {
            if let Err(e) = b.ops.flush_all() {
                tracing::warn!(bucket = %b.name, errno = e, "flush at shutdown failed");
            }
            b.ops.close_session();
        }
    });
    Ok(())
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "atlas_native_s3=info,atlas_native_fuse=info,s3s=warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    // reqwest's rustls backend can't pick between ring and aws-lc-rs when both are compiled in.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = Args::parse();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("atlas-native-s3: runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("atlas-native-s3: {e}");
            ExitCode::FAILURE
        }
    }
}
