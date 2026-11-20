// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! A fake source connector that serves a canned schema — the DataBridge analog of `FakeCephDriver`.
//! Lets the whole migration pipeline run under `make run` with no cloud credentials.

use anyhow::Result;
use async_trait::async_trait;

use crate::connector::{DiscoveredSchema, SourceConnector, SourceKind, TableInfo};

pub struct FakeSourceConnector {
    source_id: String,
    kind: SourceKind,
}

impl FakeSourceConnector {
    pub fn new(source_id: impl Into<String>, kind: SourceKind) -> Self {
        Self {
            source_id: source_id.into(),
            kind,
        }
    }
}

#[async_trait]
impl SourceConnector for FakeSourceConnector {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    async fn discover(&self) -> Result<DiscoveredSchema> {
        // A small, realistic e-commerce-ish schema. One table intentionally lacks a PK so the
        // assessment step has a blocker to surface in demos.
        let (engine, version, extensions, schema_ns) = match self.kind {
            SourceKind::Postgres => (
                "postgres",
                "15.5",
                vec!["uuid-ossp".to_string(), "pg_stat_statements".to_string()],
                "public",
            ),
            SourceKind::Mysql => ("mysql", "8.0.36", vec![], "appdb"),
            SourceKind::Mariadb => ("mariadb", "11.4.2", vec![], "appdb"),
            // Oracle / SQL Server are heterogeneous sources; the canned schema uses the source's
            // native namespace so the discovered shape looks realistic in the console.
            SourceKind::Oracle => (
                "oracle",
                "19.0.0.0.0",
                vec!["ORA_SUPPLEMENTAL_LOG".to_string()],
                "APP",
            ),
            SourceKind::Sqlserver => ("sqlserver", "16.0.4125", vec!["cdc".to_string()], "dbo"),
            // MongoDB: "tables" are collections and the schema namespace is the database name.
            SourceKind::Mongodb => (
                "mongodb",
                "7.0.12",
                vec!["replicaSet:rs0".to_string()],
                "appdb",
            ),
        };
        let tables = vec![
            TableInfo {
                schema: schema_ns.into(),
                name: "customers".into(),
                est_rows: 1_240_000,
                size_bytes: 512 * 1024 * 1024,
                has_primary_key: true,
            },
            TableInfo {
                schema: schema_ns.into(),
                name: "orders".into(),
                est_rows: 8_900_000,
                size_bytes: 3 * 1024 * 1024 * 1024,
                has_primary_key: true,
            },
            TableInfo {
                schema: schema_ns.into(),
                name: "order_items".into(),
                est_rows: 41_000_000,
                size_bytes: 9 * 1024 * 1024 * 1024,
                has_primary_key: true,
            },
            TableInfo {
                schema: schema_ns.into(),
                name: "audit_log".into(),
                est_rows: 15_000_000,
                size_bytes: 2 * 1024 * 1024 * 1024,
                has_primary_key: false,
            },
        ];
        let total_size_bytes = tables.iter().map(|t| t.size_bytes).sum();
        Ok(DiscoveredSchema {
            engine: engine.to_string(),
            version: version.to_string(),
            databases: vec!["appdb".to_string()],
            tables,
            extensions,
            total_size_bytes,
            cdc_capable: true,
        })
    }
}
