// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! The source-connector contract: discover a cloud/source database's schema so the pipeline can
//! assess readiness, provision an edge target, and plan the load. Mirrors the `StorageDriver`
//! trait split — the gateway/pipeline depend only on this trait, never on tokio-postgres/mysql.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Source database engine. Postgres/MySQL/MariaDB migrate homogeneously to a like-for-like edge
/// engine; Oracle/SQL Server migrate heterogeneously to a Postgres edge (Debezium captures the
/// source, the JDBC sink applies to Postgres — the initial Debezium snapshot seeds the edge).
/// MongoDB is the one document engine: it migrates homogeneously to a Percona Server for MongoDB
/// edge (Debezium Mongo change streams → the MongoDB Kafka sink).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Postgres,
    Mysql,
    Mariadb,
    Oracle,
    Sqlserver,
    Mongodb,
}

/// The edge operator/engine a source migrates onto. Postgres-family and the heterogeneous
/// Oracle/SQL Server sources land on CloudNativePG; MySQL/MariaDB land on Percona XtraDB; MongoDB
/// lands on Percona Server for MongoDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeOperator {
    /// CloudNativePG — the Postgres edge target.
    Cnpg,
    /// Percona XtraDB Cluster — the MySQL edge target.
    Percona,
    /// Percona Server for MongoDB — the document edge target.
    Psmdb,
}

impl EdgeOperator {
    /// Edge database engine string stored on the `edge_db_clusters` row / matched by the reconciler.
    pub fn engine_str(&self) -> &'static str {
        match self {
            Self::Cnpg => "postgres",
            Self::Percona => "mysql",
            Self::Psmdb => "mongodb",
        }
    }
    /// Operator label stored on the edge cluster row.
    pub fn operator_str(&self) -> &'static str {
        match self {
            Self::Cnpg => "cnpg",
            Self::Percona => "percona",
            Self::Psmdb => "psmdb",
        }
    }
}

impl SourceKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "pg" => Some(Self::Postgres),
            "mysql" => Some(Self::Mysql),
            "mariadb" | "maria" => Some(Self::Mariadb),
            "oracle" | "ora" => Some(Self::Oracle),
            "sqlserver" | "mssql" | "sql-server" | "sqlsrv" => Some(Self::Sqlserver),
            "mongodb" | "mongo" => Some(Self::Mongodb),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
            Self::Mariadb => "mariadb",
            Self::Oracle => "oracle",
            Self::Sqlserver => "sqlserver",
            Self::Mongodb => "mongodb",
        }
    }

    /// Whether this is a document (NoSQL) engine rather than a relational one. Document sources use a
    /// MongoDB edge + Mongo Kafka sink instead of a JDBC sink.
    pub fn is_document(&self) -> bool {
        matches!(self, Self::Mongodb)
    }

    /// Which edge operator this source migrates onto.
    pub fn edge_operator(&self) -> EdgeOperator {
        match self {
            Self::Postgres | Self::Oracle | Self::Sqlserver => EdgeOperator::Cnpg,
            Self::Mysql | Self::Mariadb => EdgeOperator::Percona,
            Self::Mongodb => EdgeOperator::Psmdb,
        }
    }

    /// Homogeneous migration — the edge engine is the same family as the source, so a dump→restore
    /// full-load applies. Heterogeneous sources (Oracle/SQL Server → Postgres) are instead seeded by
    /// Debezium's initial snapshot and have no separate full-load Job.
    pub fn homogeneous(&self) -> bool {
        matches!(
            self,
            Self::Postgres | Self::Mysql | Self::Mariadb | Self::Mongodb
        )
    }

    /// Default network port for the engine, used when the source didn't specify one.
    pub fn default_port(&self) -> i64 {
        match self {
            Self::Postgres => 5432,
            Self::Mysql | Self::Mariadb => 3306,
            Self::Oracle => 1521,
            Self::Sqlserver => 1433,
            Self::Mongodb => 27017,
        }
    }

    /// The Debezium connector class that captures this source engine.
    pub fn debezium_class(&self) -> &'static str {
        match self {
            Self::Postgres => "io.debezium.connector.postgresql.PostgresConnector",
            Self::Mysql => "io.debezium.connector.mysql.MySqlConnector",
            Self::Mariadb => "io.debezium.connector.mariadb.MariaDbConnector",
            Self::Oracle => "io.debezium.connector.oracle.OracleConnector",
            Self::Sqlserver => "io.debezium.connector.sqlserver.SqlServerConnector",
            Self::Mongodb => "io.debezium.connector.mongodb.MongoDbConnector",
        }
    }
}

/// Splits an Oracle source `database` into the container database Debezium connects to and the
/// pluggable database that holds the tables: `CDB/PDB` (e.g. `FREE/FREEPDB1`) for a multitenant
/// Oracle, a bare service name for a non-CDB one.
pub fn oracle_containers(database: &str) -> (&str, Option<&str>) {
    match database.split_once('/') {
        Some((cdb, pdb)) if !cdb.is_empty() && !pdb.is_empty() => (cdb, Some(pdb)),
        _ => (database, None),
    }
}

/// Which managed-cloud flavor the source is (affects TLS/auth/privilege preflight only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceCloud {
    Rds,
    Aurora,
    CloudSql,
    Generic,
}

impl SourceCloud {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "rds" => Self::Rds,
            "aurora" => Self::Aurora,
            "cloudsql" | "cloud-sql" => Self::CloudSql,
            _ => Self::Generic,
        }
    }
}

/// One table discovered on the source, with the facts assessment needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub est_rows: i64,
    pub size_bytes: i64,
    /// CDC needs a primary key (or replica identity) — a missing PK is an assessment blocker.
    pub has_primary_key: bool,
}

/// The full schema snapshot returned by a discovery pass. Serialized into `migration_sources.discovered`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredSchema {
    pub engine: String,
    pub version: String,
    pub databases: Vec<String>,
    pub tables: Vec<TableInfo>,
    /// Extensions (postgres) / plugins (mysql) present on the source.
    pub extensions: Vec<String>,
    pub total_size_bytes: i64,
    /// Whether the server has logical decoding / binlog enabled for CDC.
    pub cdc_capable: bool,
}

/// A backend that can introspect one source database.
#[async_trait]
pub trait SourceConnector: Send + Sync {
    /// The registered source id this connector serves.
    fn source_id(&self) -> &str;
    /// Full discovery pass: engine/version + databases + tables + sizes + CDC capability.
    async fn discover(&self) -> Result<DiscoveredSchema>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_covers_all_aliases() {
        assert_eq!(SourceKind::parse("PostgreSQL"), Some(SourceKind::Postgres));
        assert_eq!(SourceKind::parse("mysql"), Some(SourceKind::Mysql));
        assert_eq!(SourceKind::parse("MariaDB"), Some(SourceKind::Mariadb));
        assert_eq!(SourceKind::parse("oracle"), Some(SourceKind::Oracle));
        assert_eq!(SourceKind::parse("mssql"), Some(SourceKind::Sqlserver));
        assert_eq!(SourceKind::parse("sql-server"), Some(SourceKind::Sqlserver));
        assert_eq!(SourceKind::parse("mongodb"), Some(SourceKind::Mongodb));
        assert_eq!(SourceKind::parse("mongo"), Some(SourceKind::Mongodb));
        assert_eq!(SourceKind::parse("cassandra"), None);
    }

    #[test]
    fn edge_operator_and_homogeneity() {
        assert_eq!(SourceKind::Postgres.edge_operator(), EdgeOperator::Cnpg);
        assert_eq!(SourceKind::Mysql.edge_operator(), EdgeOperator::Percona);
        assert_eq!(SourceKind::Mariadb.edge_operator(), EdgeOperator::Percona);
        // Oracle / SQL Server are heterogeneous — they land on a Postgres (CNPG) edge.
        assert_eq!(SourceKind::Oracle.edge_operator(), EdgeOperator::Cnpg);
        assert_eq!(SourceKind::Sqlserver.edge_operator(), EdgeOperator::Cnpg);
        // MongoDB is a homogeneous document migration onto Percona Server for MongoDB.
        assert_eq!(SourceKind::Mongodb.edge_operator(), EdgeOperator::Psmdb);
        assert!(SourceKind::Mongodb.is_document());
        assert!(!SourceKind::Postgres.is_document());
        assert!(SourceKind::Mariadb.homogeneous());
        assert!(SourceKind::Mongodb.homogeneous());
        assert!(!SourceKind::Oracle.homogeneous());
        assert!(!SourceKind::Sqlserver.homogeneous());
    }

    #[test]
    fn debezium_classes_and_ports() {
        assert!(SourceKind::Mariadb.debezium_class().contains("mariadb"));
        assert!(SourceKind::Oracle.debezium_class().contains("oracle"));
        assert!(SourceKind::Sqlserver.debezium_class().contains("sqlserver"));
        assert!(SourceKind::Mongodb.debezium_class().contains("mongodb"));
        assert_eq!(SourceKind::Oracle.default_port(), 1521);
        assert_eq!(SourceKind::Sqlserver.default_port(), 1433);
        assert_eq!(SourceKind::Mongodb.default_port(), 27017);
    }

    #[test]
    fn oracle_database_names_cdb_and_pdb() {
        assert_eq!(
            oracle_containers("FREE/FREEPDB1"),
            ("FREE", Some("FREEPDB1"))
        );
        assert_eq!(oracle_containers("ORCL"), ("ORCL", None));
        assert_eq!(oracle_containers("/PDB"), ("/PDB", None));
    }
}
