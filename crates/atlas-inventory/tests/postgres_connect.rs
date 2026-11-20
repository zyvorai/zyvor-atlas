// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Phase-1 HA smoke: `connect`/`migrate` dispatch on URL scheme, and work against real Postgres
//! when `DATABASE_URL` is set.
//!
//! ```bash
//! # optional lab DB:
//! docker compose -f deploy/postgres/docker-compose.yml up -d
//! DATABASE_URL=postgres://atlas:atlas@127.0.0.1:5432/atlas \
//!   cargo test -p atlas-inventory --test postgres_connect -- --ignored
//! ```
//!
//! Without Docker / `DATABASE_URL`, the ignored test is skipped; default `cargo test` still
//! checks the scheme-detection helper `connect`/`migrate` dispatch on.

use atlas_inventory::{connect, is_postgres_url, migrate};

#[test]
fn is_postgres_url_detects_scheme() {
    assert!(is_postgres_url(
        "postgres://atlas:atlas@127.0.0.1:5432/atlas"
    ));
    assert!(is_postgres_url(
        "postgresql://atlas:atlas@127.0.0.1:5432/atlas"
    ));
    assert!(is_postgres_url(
        "POSTGRES://atlas:atlas@127.0.0.1:5432/atlas"
    ));
    assert!(!is_postgres_url("sqlite:///tmp/atlas.db?mode=rwc"));
}

#[tokio::test]
#[ignore = "requires live Postgres; set DATABASE_URL and run with --ignored"]
async fn connect_and_migrate_postgres() {
    let url = std::env::var("DATABASE_URL")
        .or_else(|_| std::env::var("ATLAS_DATABASE_URL"))
        .expect("DATABASE_URL or ATLAS_DATABASE_URL must be set for this ignored test");
    let pool = connect(&url)
        .await
        .unwrap_or_else(|e| panic!("connect failed: {e}"));
    migrate(&pool, &url)
        .await
        .unwrap_or_else(|e| panic!("migrate failed: {e}"));
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage_backends")
        .fetch_one(&pool)
        .await
        .expect("probe query after migrate");
    assert!(n >= 0);
}
