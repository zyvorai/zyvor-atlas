// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Live Microsoft SQL Server source connector (Azure SQL / RDS SQL Server / any TDS endpoint).
//! Introspects the schema over a real `tiberius` (pure-Rust TDS) connection. SQL Server is a
//! *heterogeneous* source: it migrates onto a Postgres edge, seeded by Debezium's initial snapshot.
//!
//! Behind the `sqlserver` cargo feature because `tiberius` pulls in a TLS stack. CDC capability is
//! `sys.databases.is_cdc_enabled` for the target database (SQL Server Change Data Capture).

use anyhow::{Context, Result};
use async_trait::async_trait;
use tiberius::{AuthMethod, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use crate::connector::{DiscoveredSchema, SourceConnector, TableInfo};

pub struct SqlServerSourceConnector {
    source_id: String,
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
    tls_mode: String,
}

impl SqlServerSourceConnector {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source_id: impl Into<String>,
        host: &str,
        port: i64,
        database: &str,
        user: &str,
        password: &str,
        tls_mode: &str,
    ) -> Self {
        Self {
            source_id: source_id.into(),
            host: host.to_string(),
            port: port.clamp(1, 65535) as u16,
            database: database.to_string(),
            user: user.to_string(),
            password: password.to_string(),
            tls_mode: tls_mode.to_string(),
        }
    }

    fn config(&self) -> Config {
        let mut config = Config::new();
        config.host(&self.host);
        config.port(self.port);
        config.database(&self.database);
        config.authentication(AuthMethod::sql_server(&self.user, &self.password));
        let level = match self.tls_mode.to_ascii_lowercase().as_str() {
            "disable" | "disabled" | "off" => EncryptionLevel::NotSupported,
            "prefer" | "preferred" => EncryptionLevel::On,
            _ => EncryptionLevel::Required,
        };
        config.encryption(level);
        // Cloud endpoints without a locally-trusted CA (the common lab case): trust the presented
        // cert unless the source explicitly asked for full verification.
        if !self.tls_mode.eq_ignore_ascii_case("verify-full") {
            config.trust_cert();
        }
        config
    }
}

#[async_trait]
impl SourceConnector for SqlServerSourceConnector {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    async fn discover(&self) -> Result<DiscoveredSchema> {
        let config = self.config();
        let tcp = TcpStream::connect(config.get_addr())
            .await
            .context("connect to source SQL Server")?;
        tcp.set_nodelay(true).ok();
        let mut client = tiberius::Client::connect(config, tcp.compat_write())
            .await
            .context("TDS handshake with source SQL Server")?;

        let version: String = client
            .query(
                "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128))",
                &[],
            )
            .await?
            .into_first_result()
            .await?
            .first()
            .and_then(|r| r.get::<&str, _>(0).map(|s| s.to_string()))
            .unwrap_or_default();

        let cdc_capable: bool = client
            .query(
                "SELECT is_cdc_enabled FROM sys.databases WHERE name = DB_NAME()",
                &[],
            )
            .await?
            .into_first_result()
            .await?
            .first()
            .and_then(|r| r.get::<bool, _>(0))
            .unwrap_or(false);

        let databases: Vec<String> = client
            .query(
                "SELECT name FROM sys.databases WHERE database_id > 4 ORDER BY name",
                &[],
            )
            .await?
            .into_first_result()
            .await?
            .iter()
            .filter_map(|r| r.get::<&str, _>(0).map(|s| s.to_string()))
            .collect();

        let rows = client
            .query(
                "SELECT s.name AS schema_name, t.name AS table_name, \
                        CAST(SUM(p.rows) AS bigint) AS est_rows, \
                        CAST(SUM(a.total_pages) * 8 * 1024 AS bigint) AS size_bytes, \
                        CASE WHEN EXISTS(SELECT 1 FROM sys.indexes i \
                             WHERE i.object_id = t.object_id AND i.is_primary_key = 1) \
                             THEN 1 ELSE 0 END AS has_pk \
                 FROM sys.tables t \
                 JOIN sys.schemas s ON s.schema_id = t.schema_id \
                 JOIN sys.partitions p ON p.object_id = t.object_id AND p.index_id IN (0,1) \
                 JOIN sys.allocation_units a ON a.container_id = p.partition_id \
                 WHERE t.is_ms_shipped = 0 AND s.name NOT IN ('cdc','sys') \
                 GROUP BY s.name, t.name, t.object_id \
                 ORDER BY 1,2",
                &[],
            )
            .await?
            .into_first_result()
            .await
            .context("introspect tables")?;
        let tables: Vec<TableInfo> = rows
            .iter()
            .map(|r| TableInfo {
                schema: r
                    .get::<&str, _>("schema_name")
                    .unwrap_or_default()
                    .to_string(),
                name: r
                    .get::<&str, _>("table_name")
                    .unwrap_or_default()
                    .to_string(),
                est_rows: r.get::<i64, _>("est_rows").unwrap_or(0).max(0),
                size_bytes: r.get::<i64, _>("size_bytes").unwrap_or(0).max(0),
                has_primary_key: r.get::<i32, _>("has_pk").unwrap_or(0) != 0,
            })
            .collect();
        let total_size_bytes = tables.iter().map(|t| t.size_bytes).sum();

        Ok(DiscoveredSchema {
            engine: "sqlserver".to_string(),
            version,
            databases: if databases.is_empty() {
                vec![self.database.clone()]
            } else {
                databases
            },
            tables,
            extensions: vec![],
            total_size_bytes,
            cdc_capable,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::SourceConnector;

    /// Integration test against a real SQL Server. Set `DATABRIDGE_TEST_MSSQL` to
    /// `host,port,database,user,password` (see `scripts/test-connectors.sh`); skipped when unset.
    /// Expects a `customers` table; `cdc_capable` reflects whether CDC is enabled on the database.
    #[tokio::test]
    async fn discovers_real_sqlserver() {
        let Ok(spec) = std::env::var("DATABRIDGE_TEST_MSSQL") else {
            eprintln!("skipping: set DATABRIDGE_TEST_MSSQL to run");
            return;
        };
        let p: Vec<&str> = spec.split(',').collect();
        assert_eq!(
            p.len(),
            5,
            "DATABRIDGE_TEST_MSSQL must be host,port,database,user,password"
        );
        let conn = SqlServerSourceConnector::new(
            "src_test",
            p[0],
            p[1].parse().expect("port"),
            p[2],
            p[3],
            p[4],
            "require",
        );
        let schema = conn.discover().await.expect("discover");
        assert_eq!(schema.engine, "sqlserver");
        let customers = schema
            .tables
            .iter()
            .find(|t| t.name == "customers")
            .expect("customers table");
        assert!(customers.has_primary_key, "customers should have a PK");
    }
}
