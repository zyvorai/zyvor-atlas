// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Live MySQL / MariaDB source connector (RDS/Aurora/Cloud SQL or any MySQL-wire endpoint).
//! Introspects the schema over a real `sqlx` connection so the pipeline can assess readiness for a
//! real migration. MariaDB speaks the MySQL wire protocol, so one connector serves both — only the
//! reported `engine` label differs (set from the registered `SourceKind`).
//!
//! CDC capability: MySQL/MariaDB need the binlog on (`log_bin=1`) in `ROW` format for Debezium.

use anyhow::{Context, Result};
use async_trait::async_trait;
use sqlx::mysql::{MySqlConnectOptions, MySqlSslMode};
use sqlx::{Connection, Row};

use crate::connector::{DiscoveredSchema, SourceConnector, TableInfo};

/// System schemas excluded from discovery.
const SYSTEM_SCHEMAS: &str = "'mysql','information_schema','performance_schema','sys'";

pub struct MysqlSourceConnector {
    source_id: String,
    /// Reported engine label — `mysql` or `mariadb` (the wire protocol is identical).
    engine: &'static str,
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
    ssl_mode: MySqlSslMode,
}

/// Map a source's `tls_mode` string onto a `sqlx` MySQL SSL mode. Cloud endpoints usually require
/// SSL; the default here mirrors the source's declared mode (falling back to `Preferred`).
fn ssl_mode(tls_mode: &str) -> MySqlSslMode {
    match tls_mode.to_ascii_lowercase().as_str() {
        "disable" | "disabled" | "off" => MySqlSslMode::Disabled,
        "prefer" | "preferred" => MySqlSslMode::Preferred,
        "require" | "required" => MySqlSslMode::Required,
        "verify-ca" | "verify_ca" => MySqlSslMode::VerifyCa,
        "verify-full" | "verify_identity" | "verify-identity" => MySqlSslMode::VerifyIdentity,
        _ => MySqlSslMode::Preferred,
    }
}

impl MysqlSourceConnector {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source_id: impl Into<String>,
        engine: &'static str,
        host: &str,
        port: i64,
        database: &str,
        user: &str,
        password: &str,
        tls_mode: &str,
    ) -> Self {
        Self {
            source_id: source_id.into(),
            engine,
            host: host.to_string(),
            port: port.clamp(1, 65535) as u16,
            database: database.to_string(),
            user: user.to_string(),
            password: password.to_string(),
            ssl_mode: ssl_mode(tls_mode),
        }
    }

    fn connect_options(&self) -> MySqlConnectOptions {
        MySqlConnectOptions::new()
            .host(&self.host)
            .port(self.port)
            .database(&self.database)
            .username(&self.user)
            .password(&self.password)
            .ssl_mode(self.ssl_mode)
    }
}

#[async_trait]
impl SourceConnector for MysqlSourceConnector {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    async fn discover(&self) -> Result<DiscoveredSchema> {
        let mut conn = sqlx::mysql::MySqlConnection::connect_with(&self.connect_options())
            .await
            .context("connect to source MySQL/MariaDB")?;

        let version: String = sqlx::query("SELECT VERSION()")
            .fetch_one(&mut conn)
            .await
            .context("read server version")?
            .try_get(0)?;

        // Binlog on + ROW format => Debezium-capable.
        let log_bin: i64 = sqlx::query("SELECT @@log_bin")
            .fetch_one(&mut conn)
            .await
            .map(|r| r.try_get::<i64, _>(0).unwrap_or(0))
            .unwrap_or(0);
        let binlog_format: String = sqlx::query("SELECT @@binlog_format")
            .fetch_one(&mut conn)
            .await
            .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
            .unwrap_or_default();
        let cdc_capable = log_bin == 1 && binlog_format.eq_ignore_ascii_case("ROW");

        // NB: information_schema string columns use a binary-ish collation that sqlx-mysql won't
        // decode as `String` directly — CAST them to CHAR or the names come back empty.
        // Only the `SYSTEM_SCHEMAS` const is interpolated (never source-DB-controlled data).
        let databases: Vec<String> = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT CAST(schema_name AS CHAR) AS s FROM information_schema.schemata \
             WHERE schema_name NOT IN ({SYSTEM_SCHEMAS}) ORDER BY 1"
        )))
        .fetch_all(&mut conn)
        .await
        .context("list databases")?
        .iter()
        .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
        .collect();

        // Active plugins stand in for Postgres "extensions" in the assessment.
        let extensions: Vec<String> = sqlx::query(
            "SELECT CAST(plugin_name AS CHAR) AS s FROM information_schema.plugins \
             WHERE plugin_status='ACTIVE' AND plugin_type='STORAGE ENGINE' ORDER BY 1",
        )
        .fetch_all(&mut conn)
        .await
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.try_get::<String, _>(0).ok())
                .collect()
        })
        .unwrap_or_default();

        // Only the `SYSTEM_SCHEMAS` const is interpolated (never source-DB-controlled data).
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT CAST(t.table_schema AS CHAR) AS s, CAST(t.table_name AS CHAR) AS n, \
                    CAST(COALESCE(t.table_rows,0) AS SIGNED) AS est_rows, \
                    CAST(COALESCE(t.data_length,0)+COALESCE(t.index_length,0) AS SIGNED) AS size_bytes, \
                    CAST(EXISTS(SELECT 1 FROM information_schema.table_constraints tc \
                                WHERE tc.table_schema=t.table_schema AND tc.table_name=t.table_name \
                                  AND tc.constraint_type='PRIMARY KEY') AS SIGNED) AS has_pk \
             FROM information_schema.tables t \
             WHERE t.table_type='BASE TABLE' AND t.table_schema NOT IN ({SYSTEM_SCHEMAS}) \
             ORDER BY 1,2"
        )))
        .fetch_all(&mut conn)
        .await
        .context("introspect tables")?;
        let tables: Vec<TableInfo> = rows
            .iter()
            .map(|r| TableInfo {
                schema: r.try_get::<String, _>("s").unwrap_or_default(),
                name: r.try_get::<String, _>("n").unwrap_or_default(),
                est_rows: r.try_get::<i64, _>("est_rows").unwrap_or(0).max(0),
                size_bytes: r.try_get::<i64, _>("size_bytes").unwrap_or(0).max(0),
                has_primary_key: r.try_get::<i64, _>("has_pk").unwrap_or(0) != 0,
            })
            .collect();
        let total_size_bytes = tables.iter().map(|t| t.size_bytes).sum();

        let _ = conn.close().await;

        Ok(DiscoveredSchema {
            engine: self.engine.to_string(),
            version,
            databases: if databases.is_empty() {
                vec![self.database.clone()]
            } else {
                databases
            },
            tables,
            extensions,
            total_size_bytes,
            cdc_capable,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssl_mode_maps_source_tls() {
        // MySqlSslMode doesn't derive PartialEq, so match on the variant.
        assert!(matches!(ssl_mode("require"), MySqlSslMode::Required));
        assert!(matches!(ssl_mode("disable"), MySqlSslMode::Disabled));
        assert!(matches!(
            ssl_mode("verify-full"),
            MySqlSslMode::VerifyIdentity
        ));
        // unknown falls back to Preferred
        assert!(matches!(ssl_mode("banana"), MySqlSslMode::Preferred));
    }

    #[test]
    fn engine_label_is_preserved() {
        let c = MysqlSourceConnector::new("s1", "mariadb", "h", 3306, "appdb", "u", "p", "require");
        assert_eq!(c.engine, "mariadb");
        assert_eq!(c.port, 3306);
    }

    /// Integration tests against a real MySQL/MariaDB. Set `DATABRIDGE_TEST_MYSQL` /
    /// `DATABRIDGE_TEST_MARIADB` to `host,port,database,user,password` (see
    /// `scripts/test-connectors.sh`); skipped when unset. Expects a `customers` table with a PK and
    /// the binlog on in ROW format (`cdc_capable`).
    async fn run_real_discovery(env: &str, engine: &'static str) {
        let Ok(spec) = std::env::var(env) else {
            eprintln!("skipping: set {env} to run");
            return;
        };
        let p: Vec<&str> = spec.split(',').collect();
        assert_eq!(p.len(), 5, "{env} must be host,port,database,user,password");
        let conn = MysqlSourceConnector::new(
            "src_test",
            engine,
            p[0],
            p[1].parse().expect("port"),
            p[2],
            p[3],
            p[4],
            "disable",
        );
        let schema = conn.discover().await.expect("discover");
        assert_eq!(schema.engine, engine);
        let customers = schema
            .tables
            .iter()
            .find(|t| t.name == "customers")
            .expect("customers table");
        assert!(customers.has_primary_key, "customers should have a PK");
        assert!(
            schema.cdc_capable,
            "binlog should be ROW-format enabled for CDC"
        );
    }

    #[tokio::test]
    async fn discovers_real_mysql() {
        run_real_discovery("DATABRIDGE_TEST_MYSQL", "mysql").await;
    }

    #[tokio::test]
    async fn discovers_real_mariadb() {
        run_real_discovery("DATABRIDGE_TEST_MARIADB", "mariadb").await;
    }
}
