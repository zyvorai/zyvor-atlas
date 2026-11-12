// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Source connectors. `fake` serves a canned schema so the whole pipeline runs with no cloud creds;
//! the rest introspect a live cloud/source database. `postgres` (tokio-postgres) and `mysql` (sqlx,
//! also serving MariaDB) are always built; `sqlserver` (tiberius) and `oracle` (OCI) are behind the
//! `sqlserver` / `oracle` cargo features because they pull in TLS / native-client dependencies.

pub mod fake;
pub mod mysql;
pub mod oracle_dsn;
pub mod postgres;

#[cfg(feature = "mongodb")]
pub mod mongodb;
#[cfg(feature = "oracle")]
pub mod oracle;
#[cfg(feature = "sqlserver")]
pub mod sqlserver;
