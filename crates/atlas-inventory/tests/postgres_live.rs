// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Live verification for the HA migration (docs/HA.md): proves `connect()`/`migrate()` work
//! against a real Postgres via the shared `sqlx::Any` driver, not just that they compile.
//!
//! Opt-in (depends on external infra, `cargo test` doesn't run `#[ignore]`d tests by default):
//!   deploy/postgres-lab/up.sh   # stands up a throwaway Postgres in the lab
//!   DATABASE_URL="postgres://atlas:<password>@<node-ip>:30432/atlas" \
//!     cargo test -p atlas-inventory --test postgres_live -- --ignored --nocapture

use sqlx::Row;

fn lab_database_url() -> String {
    std::env::var("DATABASE_URL").expect(
        "set DATABASE_URL to the lab Postgres, e.g. \
         postgres://atlas:<password>@<node-ip>:30432/atlas — see deploy/postgres-lab/README.md \
         for how to fetch the generated password",
    )
}

#[tokio::test]
#[ignore]
async fn connect_and_migrate_against_real_postgres() {
    let url = lab_database_url();

    let pool = atlas_inventory::connect(&url)
        .await
        .expect("connect should open a real connection to the lab Postgres");

    atlas_inventory::migrate(&pool, &url)
        .await
        .expect("migrate should apply migrations-postgres/ cleanly");

    // Re-running migrate is idempotent (sqlx tracks applied versions in _sqlx_migrations) — this
    // is what a pod restart / rolling redeploy does on every boot in the real code path.
    atlas_inventory::migrate(&pool, &url)
        .await
        .expect("migrate must be safe to run again against an already-migrated database");

    // Spot-check a handful of tables spanning the migration history, including the three ported
    // in this session (rbd_snapshots, the native-id unique index, console_users.tenant_id) — a
    // schema-parity gap here would mean migrations-postgres/ silently fell behind migrations/
    // again.
    for table in [
        "storage_volumes",
        "storage_audit_logs",
        "rbd_snapshots",
        "console_users",
    ] {
        // `table` is one of the four fixed literals above — nothing user-controlled.
        let row = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) AS c FROM {table}")))
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| panic!("table {table} should exist and be queryable: {e}"));
        let _: i64 = row.get("c");
    }

    let tenant_col_exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_name = 'console_users' AND column_name = 'tenant_id')",
    )
    .fetch_one(&pool)
    .await
    .unwrap()
    .get(0);
    assert!(
        tenant_col_exists,
        "console_users.tenant_id must exist (migration 0028)"
    );

    // Exercise a real query-layer round trip on Postgres, not just connect+migrate: the
    // AnyPool/`$N`-placeholder conversion is what actually unblocks multi-replica HA.
    atlas_inventory::alerts::upsert_open(
        &pool,
        "pg_live_probe",
        "info",
        "test",
        "probe",
        "p1",
        "postgres live probe",
        "verifying the query layer on Postgres",
        &serde_json::json!({}),
    )
    .await
    .expect("upsert_open should run on Postgres");
    let alerts = atlas_inventory::alerts::list(&pool, Some("open"))
        .await
        .expect("list should run on Postgres");
    assert!(alerts.iter().any(|a| a.id == "pg_live_probe"));
    atlas_inventory::alerts::resolve(&pool, "pg_live_probe")
        .await
        .expect("resolve should run on Postgres");

    pool.close().await;
}

/// Exercises `crates/atlas-inventory/src/users.rs`'s case-insensitive username lookup against
/// real Postgres — SQLite's `console_users.username` PK used `COLLATE NOCASE`, which Postgres has
/// no equivalent of on a plain `TEXT` column (see migrations-postgres/0025_console_users.sql), so
/// lookups were rewritten to `lower(username) = lower($N)`. Proves that rewrite actually
/// preserves case-insensitive login on Postgres, not just that it compiles.
#[tokio::test]
#[ignore]
async fn users_case_insensitive_lookup_against_real_postgres() {
    let url = lab_database_url();
    let pool = atlas_inventory::connect(&url)
        .await
        .expect("connect should open a real connection to the lab Postgres");
    atlas_inventory::migrate(&pool, &url)
        .await
        .expect("migrate should apply migrations-postgres/ cleanly");

    let username = "PgLiveCaseProbe";
    sqlx::query("DELETE FROM console_users WHERE lower(username) = lower($1)")
        .bind(username)
        .execute(&pool)
        .await
        .expect("pre-test cleanup should run on Postgres");

    atlas_inventory::users::create(&pool, username, "hash", "admin", "global", "test")
        .await
        .expect("create should run on Postgres");

    for lookup in ["PgLiveCaseProbe", "pglivecaseprobe", "PGLIVECASEPROBE"] {
        let found = atlas_inventory::users::get(&pool, lookup)
            .await
            .unwrap_or_else(|e| panic!("get({lookup}) should run on Postgres: {e}"));
        assert!(
            found.is_some(),
            "lookup {lookup:?} must find the user regardless of case"
        );
    }

    pool.close().await;
}

/// Exercises `crates/atlas-inventory/src/jobs.rs` against real Postgres — the job engine's
/// persistence layer is the linchpin of the whole HA effort (atomic claim = safe multi-replica
/// dispatch), and it's also where the migration found a real portability bug: SQLite's 2-argument
/// scalar `MAX(a, b)` (used in `mark_running`/`try_claim`/`reclaim_stale_running`) has no Postgres
/// equivalent (`MAX()` there is aggregate-only) — rewritten to `CASE WHEN a > b THEN a ELSE b
/// END`. This test would fail with a Postgres `function max(integer, integer) does not exist`
/// error if that fix ever regressed.
#[tokio::test]
#[ignore]
async fn jobs_module_round_trip_against_real_postgres() {
    let url = lab_database_url();
    let pool = atlas_inventory::connect(&url)
        .await
        .expect("connect should open a real connection to the lab Postgres");
    atlas_inventory::migrate(&pool, &url)
        .await
        .expect("migrate should apply migrations-postgres/ cleanly");

    let id = "pg_live_job_probe";
    // Self-cleaning: a prior run that panicked mid-test (or a concurrent run) can leave this row
    // behind, which would otherwise fail insert_job's primary-key insert on the next run.
    sqlx::query("DELETE FROM storage_jobs WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("pre-test cleanup should run on Postgres");
    atlas_inventory::jobs::insert_job(
        &pool,
        id,
        "t",
        "volume.create",
        "pg-live-test",
        &serde_json::json!({}),
        None,
    )
    .await
    .expect("insert_job should run on Postgres");

    // try_claim exercises the CASE WHEN progress_percent bump (was MAX(progress_percent, 5)).
    let claimed = atlas_inventory::jobs::try_claim(&pool, id, "w1")
        .await
        .expect("try_claim should run on Postgres — this is where the MAX(a,b) bug surfaced");
    assert!(claimed, "first claim should win");
    let lost = atlas_inventory::jobs::try_claim(&pool, id, "w2")
        .await
        .expect("try_claim should run on Postgres");
    assert!(!lost, "second claim must lose");

    let job = atlas_inventory::jobs::get_job(&pool, id)
        .await
        .expect("get_job should run on Postgres")
        .expect("job should exist");
    assert_eq!(job.state, "running");
    assert!(job.progress_percent >= 5);

    // mark_running again exercises the same CASE WHEN bump a second time (idempotent re-entry).
    atlas_inventory::jobs::mark_running(&pool, id)
        .await
        .expect("mark_running should run on Postgres");

    // reclaim_stale_running exercises the third CASE WHEN site (locked_at/updated_at comparison).
    // `stale_secs <= 0` is a deliberate "disabled" guard (mirrors the other periodic workers'
    // `secs == 0` off-switch), not "everything is stale" — so backdate the lock directly instead
    // of relying on wall-clock elapsed time, which also avoids test flakiness.
    sqlx::query("UPDATE storage_jobs SET locked_at = $1, updated_at = $1 WHERE id = $2")
        .bind(atlas_inventory::now_rfc3339(
            chrono::Utc::now() - chrono::Duration::hours(1),
        ))
        .bind(id)
        .execute(&pool)
        .await
        .expect("backdating locked_at should run on Postgres");
    let reclaimed = atlas_inventory::jobs::reclaim_stale_running(&pool, 60)
        .await
        .expect("reclaim_stale_running should run on Postgres");
    assert!(
        reclaimed >= 1,
        "a 1-hour-old lock must be reclaimed under a 60s stale window"
    );
    let job = atlas_inventory::jobs::get_job(&pool, id)
        .await
        .expect("get_job should run on Postgres")
        .expect("job should exist");
    assert_eq!(job.state, "queued", "reclaimed job returns to queued");

    // Re-claim it now that it's queued again, then exercise bump_retry's retry accounting.
    assert!(
        atlas_inventory::jobs::try_claim(&pool, id, "w3")
            .await
            .expect("try_claim should run on Postgres")
    );
    atlas_inventory::jobs::bump_retry(&pool, id, 5)
        .await
        .expect("bump_retry should run on Postgres");
    let (retry_count, _) = atlas_inventory::jobs::retry_budget(&pool, id)
        .await
        .expect("retry_budget should run on Postgres");
    assert_eq!(retry_count, 1);

    pool.close().await;
}

/// Exercises `crates/atlas-inventory/src/rate_limit.rs`'s `SUM(count)` cluster-total query against
/// real Postgres specifically — `count`/`window_minute` were deliberately kept plain `INTEGER`
/// (not `BIGINT`) so `SUM()` returns Postgres `BIGINT` rather than `NUMERIC` (which `sqlx::Any`
/// can't decode at all — the exact bug migration 0032/0033 had to fix elsewhere in this session).
/// This proves that design choice actually holds, rather than just asserting it in a comment.
#[tokio::test]
#[ignore]
async fn rate_limit_cluster_totals_against_real_postgres() {
    let url = lab_database_url();
    let pool = atlas_inventory::connect(&url)
        .await
        .expect("connect should open a real connection to the lab Postgres");
    atlas_inventory::migrate(&pool, &url)
        .await
        .expect("migrate should apply migrations-postgres/ cleanly");

    let window = 999_999_001i64; // far in the future, never collides with a real window
    sqlx::query("DELETE FROM rate_limit_counters WHERE window_minute = $1")
        .bind(window)
        .execute(&pool)
        .await
        .expect("pre-test cleanup should run on Postgres");

    atlas_inventory::rate_limit::upsert_replica_count(&pool, "pg_live_actor", window, "r-a", 40)
        .await
        .expect("upsert_replica_count should run on Postgres");
    atlas_inventory::rate_limit::upsert_replica_count(&pool, "pg_live_actor", window, "r-b", 35)
        .await
        .expect("upsert_replica_count should run on Postgres");

    let totals = atlas_inventory::rate_limit::cluster_totals(&pool, window)
        .await
        .expect("cluster_totals (SUM over plain INTEGER) should decode fine on Postgres");
    let total = totals
        .iter()
        .find(|(a, _)| a == "pg_live_actor")
        .map(|(_, n)| *n);
    assert_eq!(total, Some(75), "40 (r-a) + 35 (r-b)");

    pool.close().await;
}

/// Regression: Postgres used a plain TEXT primary key, so `Admin` and `admin` could coexist while
/// all lookups match on `lower(username)`. Migration 0036 adds a unique `lower(username)` index.
#[tokio::test]
#[ignore]
async fn console_users_reject_case_variant_duplicates() {
    let url = lab_database_url();
    let pool = atlas_inventory::connect(&url).await.expect("connect");
    atlas_inventory::migrate(&pool, &url).await.expect("migrate");

    let _ = sqlx::query("DELETE FROM console_users WHERE lower(username) = 'casedup'")
        .execute(&pool)
        .await;
    atlas_inventory::users::create(&pool, "CaseDup", "h", "viewer", "global", "test")
        .await
        .expect("first create succeeds");
    let err = atlas_inventory::users::create(&pool, "casedup", "h", "admin", "global", "test")
        .await
        .expect_err("case-variant duplicate must be rejected");
    assert!(err.to_string().contains("already exists"), "got: {err}");
    let _ = sqlx::query("DELETE FROM console_users WHERE lower(username) = 'casedup'")
        .execute(&pool)
        .await;
    pool.close().await;
}
