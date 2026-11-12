// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Live Oracle Database source connector (RDS for Oracle / on-prem / any OCI-reachable service).
//! Introspects the schema over a real `oracle` (OCI) connection. Oracle is a *heterogeneous* source:
//! it migrates onto a Postgres edge, seeded by Debezium's initial snapshot.
//!
//! Behind the `oracle` cargo feature because it links the Oracle Instant Client (OCI) native
//! libraries. The `oracle` crate is blocking, so discovery runs on a `spawn_blocking` thread.
//! CDC capability requires minimal supplemental logging (`v$database.supplemental_log_data_min`).

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::connector::{DiscoveredSchema, SourceConnector, TableInfo};

pub struct OracleSourceConnector {
    source_id: String,
    host: String,
    port: i64,
    /// Oracle service name: the PDB of a `CDB/PDB` source `database`, else the whole value.
    service: String,
    user: String,
    password: String,
    /// Source `tls_mode`. `disable` is Easy Connect; anything else is a TCPS descriptor.
    tls_mode: String,
}

impl OracleSourceConnector {
    pub fn new(
        source_id: impl Into<String>,
        host: &str,
        port: i64,
        service: &str,
        user: &str,
        password: &str,
    ) -> Self {
        Self {
            source_id: source_id.into(),
            host: host.to_string(),
            port,
            service: service.to_string(),
            user: user.to_string(),
            password: password.to_string(),
            tls_mode: "disable".into(),
        }
    }

    /// `tls_mode` from the migration source (`disable` plaintext, `tcps` / `require` /
    /// `verify-full` TCPS). See [`crate::connectors::oracle_dsn`].
    pub fn with_tls_mode(mut self, tls_mode: impl Into<String>) -> Self {
        self.tls_mode = tls_mode.into();
        self
    }
}

#[async_trait]
impl SourceConnector for OracleSourceConnector {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    async fn discover(&self) -> Result<DiscoveredSchema> {
        let host = self.host.clone();
        let port = self.port;
        let service = self.service.clone();
        let user = self.user.clone();
        let password = self.password.clone();
        let tls_mode = self.tls_mode.clone();

        // The oracle crate is blocking (OCI); keep it off the async runtime threads.
        tokio::task::spawn_blocking(move || -> Result<DiscoveredSchema> {
            let connect_string = crate::connectors::oracle_dsn::connect_string(
                &host, port, &service, &tls_mode,
            )?;
            let conn = oracle::Connection::connect(&user, &password, &connect_string)
                .context("connect to source Oracle")?;

            let version: String = conn
                .query_row_as::<String>("SELECT BANNER FROM v$version WHERE ROWNUM = 1", &[])
                .unwrap_or_else(|_| "unknown".to_string());

            let supp_log: String = conn
                .query_row_as::<String>("SELECT supplemental_log_data_min FROM v$database", &[])
                .unwrap_or_else(|_| "NO".to_string());
            let cdc_capable = !supp_log.eq_ignore_ascii_case("NO");

            let databases: Vec<String> = conn
                .query_as::<String>(
                    // `oracle_maintained='N'` is Oracle's own flag for non-internal (user) schemas —
                    // robust across versions where a hardcoded denylist misses new internal schemas
                    // (e.g. 23ai/26ai add VECSYS, DBSFWUSER, BAASSYS, GGSYS, …).
                    "SELECT username FROM all_users WHERE oracle_maintained = 'N' ORDER BY username",
                    &[],
                )
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default();

            // Restrict to user (non-Oracle-maintained) schemas — see the databases query above.
            let sql = "SELECT t.owner AS OWNER, t.table_name AS TABLE_NAME, \
                        NVL(t.num_rows, 0) AS EST_ROWS, \
                        NVL(t.blocks, 0) * 8192 AS SIZE_BYTES, \
                        CASE WHEN (SELECT COUNT(*) FROM all_constraints c \
                                   WHERE c.owner = t.owner AND c.table_name = t.table_name \
                                     AND c.constraint_type = 'P') > 0 THEN 1 ELSE 0 END AS HAS_PK \
                 FROM all_tables t \
                 WHERE t.owner IN (SELECT username FROM all_users WHERE oracle_maintained = 'N') \
                 ORDER BY t.owner, t.table_name";
            let mut tables = Vec::new();
            let rows = conn.query(sql, &[]).context("introspect Oracle tables")?;
            for row_result in rows {
                let row = row_result?;
                tables.push(TableInfo {
                    schema: row.get::<&str, String>("OWNER").unwrap_or_default(),
                    name: row.get::<&str, String>("TABLE_NAME").unwrap_or_default(),
                    est_rows: row.get::<&str, i64>("EST_ROWS").unwrap_or(0).max(0),
                    size_bytes: row.get::<&str, i64>("SIZE_BYTES").unwrap_or(0).max(0),
                    has_primary_key: row.get::<&str, i64>("HAS_PK").unwrap_or(0) != 0,
                });
            }
            let total_size_bytes = tables.iter().map(|t| t.size_bytes).sum();

            Ok(DiscoveredSchema {
                engine: "oracle".to_string(),
                version,
                databases: if databases.is_empty() { vec![service.clone()] } else { databases },
                tables,
                extensions: vec![],
                total_size_bytes,
                cdc_capable,
            })
        })
        .await
        .context("oracle discovery task panicked")?
    }
}
