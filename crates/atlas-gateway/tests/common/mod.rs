// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Shared test-database provisioning for atlas-gateway's integration tests.
//!
//! Default (no env var set): a fresh SQLite temp-file DB per call — exactly what every test file
//! did inline before this module existed.
//!
//! Dual-backend CI (`ATLAS_TEST_DATABASE_URL` set to an admin Postgres connection string, e.g.
//! `postgres://atlas:atlas@127.0.0.1:5432/postgres`): each call instead `CREATE DATABASE`s a
//! fresh, uniquely-named Postgres database and returns a URL pointing at it — the same "one
//! throwaway DB per test" isolation the SQLite path already gives, just at the database level
//! instead of the file level (Postgres has no per-connection `:memory:`/file equivalent).
//! Nothing is dropped afterward; CI tears down the whole `postgres:16` service container instead.
//!
//! This is the whole point of the `sqlx::AnyPool` migration (see docs/HA.md): every existing test
//! body/assertion in this crate's `tests/*.rs` files runs unchanged against either backend — only
//! *this* module knows which one it's talking to.
#![allow(dead_code)] // not every test file exercises every helper here

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A process-unique, filesystem/SQL-identifier-safe suffix: the pid (unique across concurrently
/// running test *binaries* — `cargo test --workspace` runs each `tests/*.rs` file as its own
/// process) plus an atomic counter (unique across calls *within* one binary, where multiple
/// `#[tokio::test]` functions run concurrently by default).
fn unique_suffix() -> String {
    format!(
        "{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

/// Resolve a fresh, isolated database URL for one test. `label` should be short and
/// filesystem/SQL-identifier-safe (e.g. `"alerting"`, `"lifecycle"`) — purely for readability in
/// the resulting temp filename / database name; uniqueness itself comes from `unique_suffix()`,
/// so passing the same label from multiple call sites in one file is fine.
pub async fn fresh_database_url(label: &str) -> String {
    match std::env::var("ATLAS_TEST_DATABASE_URL") {
        Ok(admin_url) if !admin_url.trim().is_empty() => {
            postgres_database_url(&admin_url, label).await
        }
        _ => sqlite_database_url(label),
    }
}

fn sqlite_database_url(label: &str) -> String {
    let db = format!(
        "{}/atlas-test-{label}-{}.db",
        std::env::temp_dir().display(),
        unique_suffix(),
    );
    let _ = std::fs::remove_file(&db);
    format!("sqlite://{db}?mode=rwc")
}

async fn postgres_database_url(admin_url: &str, label: &str) -> String {
    // Postgres identifiers are case-folded and limited to 63 bytes; label + suffix comfortably
    // fits, but sanitize non-alphanumerics defensively since labels are free-form.
    let safe_label: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let db_name = format!("atlas_test_{safe_label}_{}", unique_suffix());

    // atlas_inventory::connect (not a bare sqlx::AnyPool::connect) so the Any driver is
    // installed exactly once via the crate's own idempotent install_default_drivers() call.
    let admin_pool = atlas_inventory::connect(admin_url)
        .await
        .expect("connect to ATLAS_TEST_DATABASE_URL (admin Postgres connection)");
    // `db_name` is built from a sanitized test label plus a unique suffix — test-only, never user input.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE DATABASE \"{db_name}\""
    )))
    .execute(&admin_pool)
    .await
    .unwrap_or_else(|e| panic!("CREATE DATABASE \"{db_name}\" failed: {e}"));
    admin_pool.close().await;

    // Swap the admin URL's path (its own target database, e.g. "postgres") for the freshly
    // created one — keeps whatever host/port/credentials the caller configured.
    let base = admin_url
        .rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(admin_url);
    format!("{base}/{db_name}")
}
