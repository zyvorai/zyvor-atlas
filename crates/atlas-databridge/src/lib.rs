// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Zyvor DataBridge — cloud-to-edge database migration control plane.
//!
//! Layered on Atlas: connectors introspect cloud source databases (PostgreSQL/MySQL on AWS RDS/Aurora
//! or GCP Cloud SQL), assessment scores readiness, and the pipeline provisions an edge database
//! (CloudNativePG / MySQL operator) on Ceph-backed storage, runs full-load + Debezium CDC, validates,
//! and cuts over. This crate isolates the connector/CR-templating logic (mirrors the driver crates).

use atlas_api_types::MigrationSource;

pub mod assess;
pub mod connector;
pub mod connectors;
pub mod cr;
pub mod kafka_lag;
pub mod loader;
pub mod object;
#[cfg(feature = "azure-blob")]
pub mod object_azure;
pub mod pipeline;
pub mod reconcile;
pub mod validate;

pub use connector::{
    DiscoveredSchema, EdgeOperator, SourceCloud, SourceConnector, SourceKind, TableInfo,
};

/// Build a source connector for a registered source. In `fake` driver mode this returns the canned
/// `FakeSourceConnector` (no credentials needed); `real` mode builds a live PostgreSQL connector from
/// the source endpoint + `creds` (username, password) resolved from the source's k8s Secret.
pub fn build_connector(
    source: &MigrationSource,
    creds: Option<(&str, &str)>,
) -> anyhow::Result<Box<dyn SourceConnector>> {
    let kind = SourceKind::parse(&source.kind)
        .ok_or_else(|| anyhow::anyhow!("unsupported source kind: {}", source.kind))?;
    match source.driver_mode.as_str() {
        "fake" => Ok(Box::new(connectors::fake::FakeSourceConnector::new(
            source.id.clone(),
            kind,
        ))),
        "real" => {
            let (user, password) =
                creds.ok_or_else(|| anyhow::anyhow!("real connector requires credentials"))?;
            let host = source.endpoint.as_deref().unwrap_or("");
            let port = source.port.unwrap_or_else(|| kind.default_port());
            let database = source.database.as_deref().unwrap_or("appdb");
            match kind {
                SourceKind::Postgres => Ok(Box::new(
                    connectors::postgres::PostgresSourceConnector::new(
                        source.id.clone(),
                        host,
                        port,
                        database,
                        user,
                        password,
                        &source.tls_mode,
                    ),
                )),
                SourceKind::Mysql | SourceKind::Mariadb => {
                    Ok(Box::new(connectors::mysql::MysqlSourceConnector::new(
                        source.id.clone(),
                        kind.as_str(),
                        host,
                        port,
                        database,
                        user,
                        password,
                        &source.tls_mode,
                    )))
                }
                SourceKind::Sqlserver => {
                    build_sqlserver(source, host, port, database, user, password)
                }
                SourceKind::Oracle => build_oracle(source, host, port, database, user, password),
                SourceKind::Mongodb => build_mongodb(source, host, port, database, user, password),
            }
        }
        other => anyhow::bail!("unknown driver_mode: {other}"),
    }
}

/// Build a real SQL Server connector — gated on the `sqlserver` feature (pulls in `tiberius`/TLS).
#[cfg(feature = "sqlserver")]
fn build_sqlserver(
    source: &MigrationSource,
    host: &str,
    port: i64,
    database: &str,
    user: &str,
    password: &str,
) -> anyhow::Result<Box<dyn SourceConnector>> {
    Ok(Box::new(
        connectors::sqlserver::SqlServerSourceConnector::new(
            source.id.clone(),
            host,
            port,
            database,
            user,
            password,
            &source.tls_mode,
        ),
    ))
}

#[cfg(not(feature = "sqlserver"))]
fn build_sqlserver(
    _source: &MigrationSource,
    _host: &str,
    _port: i64,
    _database: &str,
    _user: &str,
    _password: &str,
) -> anyhow::Result<Box<dyn SourceConnector>> {
    anyhow::bail!(
        "real SQL Server source connector requires building atlas-databridge with the `sqlserver` feature"
    )
}

/// Build a real Oracle connector — gated on the `oracle` feature (links the Oracle Instant Client).
#[cfg(feature = "oracle")]
fn build_oracle(
    source: &MigrationSource,
    host: &str,
    port: i64,
    database: &str,
    user: &str,
    password: &str,
) -> anyhow::Result<Box<dyn SourceConnector>> {
    let (cdb, pdb) = connector::oracle_containers(database);
    Ok(Box::new(connectors::oracle::OracleSourceConnector::new(
        source.id.clone(),
        host,
        port,
        pdb.unwrap_or(cdb),
        user,
        password,
    )))
}

#[cfg(not(feature = "oracle"))]
fn build_oracle(
    _source: &MigrationSource,
    _host: &str,
    _port: i64,
    _database: &str,
    _user: &str,
    _password: &str,
) -> anyhow::Result<Box<dyn SourceConnector>> {
    anyhow::bail!(
        "real Oracle source connector requires building atlas-databridge with the `oracle` feature (Oracle Instant Client)"
    )
}

/// Build a real MongoDB connector — gated on the `mongodb` feature (pulls in the `mongodb` driver).
#[cfg(feature = "mongodb")]
fn build_mongodb(
    source: &MigrationSource,
    host: &str,
    port: i64,
    database: &str,
    user: &str,
    password: &str,
) -> anyhow::Result<Box<dyn SourceConnector>> {
    Ok(Box::new(connectors::mongodb::MongoSourceConnector::new(
        source.id.clone(),
        host,
        port,
        database,
        user,
        password,
        &source.tls_mode,
    )))
}

#[cfg(not(feature = "mongodb"))]
fn build_mongodb(
    _source: &MigrationSource,
    _host: &str,
    _port: i64,
    _database: &str,
    _user: &str,
    _password: &str,
) -> anyhow::Result<Box<dyn SourceConnector>> {
    anyhow::bail!(
        "real MongoDB source connector requires building atlas-databridge with the `mongodb` feature"
    )
}
