// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Live MongoDB source connector (Atlas / DocumentDB / any mongod or replica set). Introspects the
//! databases + collections over a real `mongodb` (async, pure-Rust) connection so the pipeline can
//! assess readiness for a real migration.
//!
//! Behind the `mongodb` cargo feature (the driver pulls in bson + a TLS stack). "Tables" map to
//! collections; every document has an `_id`, so `has_primary_key` is always true. CDC capability
//! requires the source to be a **replica set** (Debezium reads change streams / the oplog) — a
//! standalone mongod can't serve change streams.

use anyhow::{Context, Result};
use async_trait::async_trait;
use mongodb::bson::{doc, Document};

use crate::connector::{DiscoveredSchema, SourceConnector, TableInfo};

pub struct MongoSourceConnector {
    source_id: String,
    host: String,
    port: i64,
    database: String,
    user: String,
    password: String,
    tls: bool,
}

impl MongoSourceConnector {
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
        let tls = !matches!(
            tls_mode.to_ascii_lowercase().as_str(),
            "disable" | "disabled" | "off"
        );
        Self {
            source_id: source_id.into(),
            host: host.to_string(),
            port,
            database: database.to_string(),
            user: user.to_string(),
            password: password.to_string(),
            tls,
        }
    }

    fn uri(&self) -> String {
        let tls = if self.tls { "&tls=true" } else { "" };
        // Omit the credentials block entirely for an unauthenticated server (empty user).
        if self.user.is_empty() {
            return format!(
                "mongodb://{}:{}/?{}",
                self.host,
                self.port,
                tls.trim_start_matches('&')
            );
        }
        // Credentials must be percent-encoded in the userinfo component — a raw ':', '@', '/', or
        // '%' in the password (common in generated secrets) would otherwise be parsed as a URI
        // delimiter and either break the connection string or point auth at the wrong host.
        format!(
            "mongodb://{}:{}@{}:{}/?authSource=admin{tls}",
            percent_encode_userinfo(&self.user),
            percent_encode_userinfo(&self.password),
            self.host,
            self.port
        )
    }
}

/// Percent-encode a URI userinfo component (RFC 3986): escape everything but unreserved
/// characters, so a username/password containing ':', '@', '/', '%', etc. can't be misparsed as
/// a URI delimiter.
fn percent_encode_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Read an integer field that a server command may return as i32 or i64.
fn get_int(d: &Document, key: &str) -> i64 {
    d.get_i64(key)
        .ok()
        .or_else(|| d.get_i32(key).ok().map(|v| v as i64))
        .unwrap_or(0)
}

#[async_trait]
impl SourceConnector for MongoSourceConnector {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    async fn discover(&self) -> Result<DiscoveredSchema> {
        let client = mongodb::Client::with_uri_str(self.uri())
            .await
            .context("connect to source MongoDB")?;
        let admin = client.database("admin");

        let version = admin
            .run_command(doc! { "buildInfo": 1 })
            .await
            .ok()
            .and_then(|d| d.get_str("version").ok().map(|s| s.to_string()))
            .unwrap_or_else(|| "unknown".to_string());

        // A replica set (hello.setName present) can serve change streams => Debezium-capable.
        let cdc_capable = admin
            .run_command(doc! { "hello": 1 })
            .await
            .ok()
            .map(|d| d.get_str("setName").is_ok())
            .unwrap_or(false);

        let databases: Vec<String> = client
            .list_database_names()
            .await
            .context("list databases")?
            .into_iter()
            .filter(|n| !matches!(n.as_str(), "admin" | "local" | "config"))
            .collect();

        // Collections of the target database, with document counts + sizes from collStats.
        let db = client.database(&self.database);
        let collections = db
            .list_collection_names()
            .await
            .context("list collections")?;
        let mut tables = Vec::new();
        for coll in collections {
            let stats = db.run_command(doc! { "collStats": &coll }).await.ok();
            let (count, size) = stats
                .as_ref()
                .map(|s| (get_int(s, "count"), get_int(s, "size")))
                .unwrap_or((0, 0));
            tables.push(TableInfo {
                schema: self.database.clone(),
                name: coll,
                est_rows: count.max(0),
                size_bytes: size.max(0),
                has_primary_key: true, // every Mongo document has _id
            });
        }
        let total_size_bytes = tables.iter().map(|t| t.size_bytes).sum();

        Ok(DiscoveredSchema {
            engine: "mongodb".to_string(),
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

    #[test]
    fn uri_embeds_creds_and_tls() {
        let c = MongoSourceConnector::new("s1", "mongo.host", 27017, "appdb", "u", "p", "require");
        let uri = c.uri();
        assert!(uri.starts_with("mongodb://u:p@mongo.host:27017/"));
        assert!(uri.contains("tls=true"));
        let c2 = MongoSourceConnector::new("s1", "h", 27017, "appdb", "u", "p", "disable");
        assert!(!c2.uri().contains("tls=true"));
    }

    #[test]
    fn uri_percent_encodes_special_chars_in_credentials() {
        // A password containing ':', '@', or '/' must not be parsed as a URI delimiter — a raw
        // '@' here would otherwise shift the host to whatever follows it.
        let c = MongoSourceConnector::new(
            "s1",
            "mongo.host",
            27017,
            "appdb",
            "u@1",
            "p:a/s@s",
            "require",
        );
        let uri = c.uri();
        assert!(uri.starts_with("mongodb://u%401:p%3Aa%2Fs%40s@mongo.host:27017/"));
    }

    /// Integration test against a real MongoDB replica set. Set `DATABRIDGE_TEST_MONGO` to
    /// `host,port,database,user,password` (see `scripts/test-connectors.sh`); skipped when unset.
    /// Expects a `customers` collection and a replica set (`cdc_capable`).
    #[tokio::test]
    async fn discovers_real_mongodb() {
        let Ok(spec) = std::env::var("DATABRIDGE_TEST_MONGO") else {
            eprintln!("skipping: set DATABRIDGE_TEST_MONGO to run");
            return;
        };
        let p: Vec<&str> = spec.split(',').collect();
        assert_eq!(
            p.len(),
            5,
            "DATABRIDGE_TEST_MONGO must be host,port,database,user,password"
        );
        let conn = MongoSourceConnector::new(
            "src_test",
            p[0],
            p[1].parse().expect("port"),
            p[2],
            p[3],
            p[4],
            "disable",
        );
        let schema = conn.discover().await.expect("discover");
        assert_eq!(schema.engine, "mongodb");
        assert!(
            schema.tables.iter().any(|t| t.name == "customers"),
            "customers collection should be discovered"
        );
        assert!(schema.cdc_capable, "a replica set should be CDC-capable");
    }
}
