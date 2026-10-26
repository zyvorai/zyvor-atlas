// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Live PostgreSQL source connector (RDS/Aurora/Cloud SQL or any PG endpoint). Introspects the
//! schema over a real connection so the pipeline can assess readiness for a real migration.
//!
//! TLS follows the source's `tls_mode`, using the libpq `sslmode` names:
//!
//! | `tls_mode` | behaviour |
//! |---|---|
//! | `disable` | plaintext |
//! | `prefer` | TLS if the server offers it (certificate not verified), else plaintext |
//! | `require` | TLS is mandatory, certificate not verified |
//! | `verify-ca`, `verify-full` | TLS is mandatory and the server certificate chain **and host name** are verified |
//!
//! `verify-ca` is treated as `verify-full` (stricter than libpq, which skips the host-name check).
//! An unknown value is treated as `require`, so a typo can never silently downgrade to plaintext.
//! Trust roots come from the platform store plus an optional PEM bundle named by
//! `ATLAS_DATABRIDGE_PG_CA_FILE`, for private CAs.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres::config::SslMode;
use tokio_postgres::Config;
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::connector::{DiscoveredSchema, SourceConnector, TableInfo};

/// Env var naming an extra PEM bundle of CA certificates to trust for `verify-*` modes.
pub const CA_FILE_ENV: &str = "ATLAS_DATABRIDGE_PG_CA_FILE";

/// How strictly the connection is secured, parsed from a source's `tls_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlsMode {
    Disable,
    Prefer,
    Require,
    Verify,
}

impl TlsMode {
    fn parse(tls_mode: &str) -> Self {
        match tls_mode.trim().to_ascii_lowercase().as_str() {
            "disable" | "disabled" | "off" => Self::Disable,
            "prefer" | "preferred" | "allow" => Self::Prefer,
            "verify-ca" | "verify_ca" | "verify-full" | "verify_full" => Self::Verify,
            // "require" and anything unrecognised: never fall back to plaintext on a typo.
            _ => Self::Require,
        }
    }

    fn ssl_mode(self) -> SslMode {
        match self {
            Self::Disable => SslMode::Disable,
            Self::Prefer => SslMode::Prefer,
            Self::Require | Self::Verify => SslMode::Require,
        }
    }
}

/// Accepts any server certificate. Used for `prefer` and `require`, which (as in libpq) encrypt
/// the connection but do not authenticate the server.
#[derive(Debug)]
struct NoVerify(Arc<CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Platform trust store plus the optional [`CA_FILE_ENV`] bundle.
fn root_store() -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for err in &native.errors {
        tracing::warn!(error = %err, "skipping an unreadable platform root certificate");
    }
    roots.add_parsable_certificates(native.certs);
    if let Ok(path) = std::env::var(CA_FILE_ENV) {
        use rustls::pki_types::pem::PemObject;
        let mut added = 0;
        for cert in CertificateDer::pem_file_iter(&path)
            .with_context(|| format!("read {CA_FILE_ENV}={path}"))?
        {
            roots
                .add(cert.with_context(|| format!("parse a certificate in {path}"))?)
                .with_context(|| format!("trust a certificate from {path}"))?;
            added += 1;
        }
        anyhow::ensure!(added > 0, "{CA_FILE_ENV}={path} contains no certificates");
    }
    Ok(roots)
}

fn tls_connector(mode: TlsMode) -> Result<MakeRustlsConnect> {
    let provider = Arc::new(ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .context("select TLS protocol versions")?;
    let config = if mode == TlsMode::Verify {
        builder
            .with_root_certificates(root_store()?)
            .with_no_client_auth()
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
            .with_no_client_auth()
    };
    Ok(MakeRustlsConnect::new(config))
}

pub struct PostgresSourceConnector {
    source_id: String,
    config: Config,
    tls: TlsMode,
}

impl PostgresSourceConnector {
    pub fn new(
        source_id: impl Into<String>,
        host: &str,
        port: i64,
        database: &str,
        user: &str,
        password: &str,
        tls_mode: &str,
    ) -> Self {
        let tls = TlsMode::parse(tls_mode);
        let mut config = Config::new();
        config
            .host(host)
            .port(u16::try_from(port).unwrap_or(5432))
            .dbname(database)
            .user(user)
            .password(password)
            .connect_timeout(Duration::from_secs(10))
            .ssl_mode(tls.ssl_mode());
        Self {
            source_id: source_id.into(),
            config,
            tls,
        }
    }

    async fn introspect(&self, client: &tokio_postgres::Client) -> Result<DiscoveredSchema> {
        let version: String = client
            .query_one("SELECT current_setting('server_version')", &[])
            .await
            .context("read server_version")?
            .get(0);
        let wal_level: String = client
            .query_one("SELECT current_setting('wal_level')", &[])
            .await
            .map(|r| r.get(0))
            .unwrap_or_else(|_| "unknown".to_string());
        let current_db: String = client
            .query_one("SELECT current_database()", &[])
            .await?
            .get(0);

        let databases: Vec<String> = client
            .query(
                "SELECT datname FROM pg_database WHERE datistemplate = false ORDER BY 1",
                &[],
            )
            .await?
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect();

        let extensions: Vec<String> = client
            .query("SELECT extname FROM pg_extension ORDER BY 1", &[])
            .await?
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect();

        let table_rows = client
            .query(
                "SELECT n.nspname AS schema, c.relname AS name, \
                        c.reltuples::bigint AS est_rows, \
                        pg_total_relation_size(c.oid) AS size_bytes, \
                        EXISTS(SELECT 1 FROM pg_index i WHERE i.indrelid = c.oid AND i.indisprimary) AS has_pk \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind = 'r' AND n.nspname NOT IN ('pg_catalog','information_schema') \
                 ORDER BY 1,2",
                &[],
            )
            .await
            .context("introspect tables")?;
        let tables: Vec<TableInfo> = table_rows
            .iter()
            .map(|r| TableInfo {
                schema: r.get("schema"),
                name: r.get("name"),
                est_rows: r.get::<_, i64>("est_rows").max(0),
                size_bytes: r.get::<_, i64>("size_bytes").max(0),
                has_primary_key: r.get("has_pk"),
            })
            .collect();
        let total_size_bytes = tables.iter().map(|t| t.size_bytes).sum();

        Ok(DiscoveredSchema {
            engine: "postgres".to_string(),
            version,
            databases: if databases.is_empty() {
                vec![current_db]
            } else {
                databases
            },
            tables,
            extensions,
            total_size_bytes,
            cdc_capable: wal_level.eq_ignore_ascii_case("logical"),
        })
    }
}

#[async_trait]
impl SourceConnector for PostgresSourceConnector {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    async fn discover(&self) -> Result<DiscoveredSchema> {
        // The connection future drives the protocol; run it until we're done introspecting.
        // `connect` is generic over the TLS type, so the two arms spawn separately.
        let (client, handle) = if self.tls == TlsMode::Disable {
            let (client, connection) = self
                .config
                .connect(tokio_postgres::NoTls)
                .await
                .context("connect to source Postgres")?;
            (
                client,
                tokio::spawn(async move {
                    let _ = connection.await;
                }),
            )
        } else {
            let (client, connection) = self
                .config
                .connect(tls_connector(self.tls)?)
                .await
                .context("connect to source Postgres over TLS")?;
            (
                client,
                tokio::spawn(async move {
                    let _ = connection.await;
                }),
            )
        };
        let result = self.introspect(&client).await;
        drop(client);
        handle.abort();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::SourceConnector;

    /// Integration test against a real Postgres. Set DATABRIDGE_TEST_PG to a libpq-style host/db
    /// (e.g. "host=127.0.0.1 port=5599 dbname=appdb user=postgres password=x"); skipped otherwise.
    #[tokio::test]
    async fn discovers_real_postgres() {
        let Ok(spec) = std::env::var("DATABRIDGE_TEST_PG") else {
            eprintln!("skipping: set DATABRIDGE_TEST_PG to run");
            return;
        };
        // spec is a full libpq-style conn string.
        let mut config: Config = spec.parse().expect("DATABRIDGE_TEST_PG parses");
        // DATABRIDGE_TEST_PG_TLS picks the tls_mode under test (default: plaintext).
        let tls = TlsMode::parse(
            &std::env::var("DATABRIDGE_TEST_PG_TLS").unwrap_or_else(|_| "disable".into()),
        );
        config.ssl_mode(tls.ssl_mode());
        let conn = PostgresSourceConnector {
            source_id: "src_test".into(),
            tls,
            config,
        };
        let schema = conn.discover().await.expect("discover");
        assert_eq!(schema.engine, "postgres");
        assert!(schema.tables.iter().any(|t| t.name == "customers"));
        let orders = schema
            .tables
            .iter()
            .find(|t| t.name == "orders")
            .expect("orders table");
        assert!(orders.has_primary_key);
    }

    #[test]
    fn tls_mode_names_map_to_libpq_semantics() {
        assert_eq!(TlsMode::parse("disable"), TlsMode::Disable);
        assert_eq!(TlsMode::parse("OFF"), TlsMode::Disable);
        assert_eq!(TlsMode::parse("prefer"), TlsMode::Prefer);
        assert_eq!(TlsMode::parse("require"), TlsMode::Require);
        assert_eq!(TlsMode::parse("verify-ca"), TlsMode::Verify);
        assert_eq!(TlsMode::parse(" Verify-Full "), TlsMode::Verify);
    }

    #[test]
    fn unknown_tls_mode_never_downgrades_to_plaintext() {
        assert_eq!(TlsMode::parse("banana"), TlsMode::Require);
        assert_eq!(TlsMode::parse(""), TlsMode::Require);
        assert_eq!(TlsMode::parse("banana").ssl_mode(), SslMode::Require);
    }

    #[test]
    fn config_carries_the_requested_ssl_mode_and_credentials_verbatim() {
        // A password with spaces and quotes must not be re-parsed as extra conn-string keys.
        let c = PostgresSourceConnector::new(
            "s",
            "db.example",
            5432,
            "app",
            "u",
            "p w'd=1 sslmode=disable",
            "verify-full",
        );
        assert_eq!(c.config.get_ssl_mode(), SslMode::Require);
        assert_eq!(
            c.config.get_password(),
            Some(&b"p w'd=1 sslmode=disable"[..])
        );
        let off = PostgresSourceConnector::new("s", "h", 5432, "d", "u", "p", "disable");
        assert_eq!(off.config.get_ssl_mode(), SslMode::Disable);
        let pref = PostgresSourceConnector::new("s", "h", 5432, "d", "u", "p", "prefer");
        assert_eq!(pref.config.get_ssl_mode(), SslMode::Prefer);
    }

    #[test]
    fn tls_connector_builds_for_every_mode() {
        for mode in [TlsMode::Prefer, TlsMode::Require, TlsMode::Verify] {
            assert!(tls_connector(mode).is_ok(), "{mode:?}");
        }
    }
}
