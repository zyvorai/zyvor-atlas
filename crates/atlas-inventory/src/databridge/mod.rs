// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! DataBridge inventory: cloud-to-edge database migration records.
//! One module per resource, each following the `buckets.rs` insert/set/get/list/row_to pattern.

pub mod cdc;
pub mod cutovers;
pub mod edge_clusters;
pub mod object_migrations;
pub mod plans;
pub mod sources;
pub mod validations;

#[cfg(test)]
mod tests {
    use crate::{connect, migrate};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_db_url(name: &str) -> String {
        let db = format!(
            "{}/atlas-databridge-mod-test-{name}-{}-{}.db",
            std::env::temp_dir().display(),
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst),
        );
        let _ = std::fs::remove_file(&db);
        format!("sqlite://{db}?mode=rwc")
    }

    #[tokio::test]
    async fn databridge_round_trip() {
        // A real file (not `sqlite::memory:`): AnyPoolOptions pools multiple connections, and
        // each fresh connection to `:memory:` gets its own separate empty database — migrate()
        // would run on one connection while later queries land on another, unmigrated one.
        let url = temp_db_url("round-trip");
        // A real file (not `sqlite::memory:`): AnyPoolOptions pools multiple connections, and
        // each fresh connection to `:memory:` gets its own separate empty database — migrate()
        // would run on one connection while later queries land on another, unmigrated one.
        let pool = connect(&url).await.unwrap();
        migrate(&pool, &url).await.unwrap();

        // source
        super::sources::insert_source(
            &pool,
            "src_test",
            "global",
            "prod-pg",
            "postgres",
            "rds",
            Some("prod.abc.rds.amazonaws.com"),
            Some(5432),
            Some("appdb"),
            Some("rds-secret"),
            Some("zyvor-databridge"),
            "require",
            "fake",
        )
        .await
        .unwrap();
        super::sources::set_discovered(&pool, "src_test", &serde_json::json!({"tables": 3}))
            .await
            .unwrap();
        let s = super::sources::get_source(&pool, "src_test")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s.state, "discovered");
        assert_eq!(s.discovered["tables"], 3);
        assert_eq!(super::sources::list_sources(&pool).await.unwrap().len(), 1);

        // plan + state transitions
        super::plans::insert_plan(
            &pool,
            "mplan_test",
            "global",
            "prod migration",
            "src_test",
            259200,
        )
        .await
        .unwrap();
        super::plans::set_assessment(
            &pool,
            "mplan_test",
            82,
            &serde_json::json!({"blockers": []}),
        )
        .await
        .unwrap();
        let p = super::plans::get_plan(&pool, "mplan_test")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.state, "assessed");
        assert_eq!(p.readiness_score, 82);

        // edge cluster + cdc lag + validation + cutover
        super::edge_clusters::insert_edge_cluster(
            &pool,
            "edb_test",
            "global",
            "mplan_test",
            "postgres",
            "cnpg",
            "zyvor-databridge",
            "pg-edge",
            "zyvor-rbd-prod",
            Some("zyvor-rbd-fast"),
            1,
        )
        .await
        .unwrap();
        super::edge_clusters::set_ready(
            &pool,
            "edb_test",
            "pg-edge.zyvor-databridge:5432",
            "pg-edge-app",
        )
        .await
        .unwrap();
        assert_eq!(
            super::edge_clusters::list_by_state(&pool, "ready")
                .await
                .unwrap()
                .len(),
            1
        );

        super::cdc::insert_stream(
            &pool,
            "cdc_test",
            "global",
            "mplan_test",
            "postgres",
            "kc",
            "dbz-pg",
            "src_test",
        )
        .await
        .unwrap();
        super::cdc::update_lag(&pool, "cdc_test", 1024, 3, Some("0/AB"), Some("0/AA"), 42)
            .await
            .unwrap();
        let c = super::cdc::get_stream(&pool, "cdc_test")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(c.lag_seconds, 3);
        assert_eq!(c.events_total, 42);

        super::validations::insert_validation(
            &pool,
            "val_test",
            "global",
            "mplan_test",
            "rowcount",
        )
        .await
        .unwrap();
        super::validations::set_result(&pool, "val_test", true, 3, 0, &serde_json::json!([]))
            .await
            .unwrap();
        assert_eq!(
            super::validations::latest_for_plan(&pool, "mplan_test")
                .await
                .unwrap()
                .unwrap()
                .state,
            "passed"
        );

        super::cutovers::insert_cutover(
            &pool,
            "cut_test",
            "global",
            "mplan_test",
            Some("src"),
            Some("edge"),
            None,
            None,
        )
        .await
        .unwrap();
        super::cutovers::set_complete(&pool, "cut_test", "complete")
            .await
            .unwrap();
        assert_eq!(
            super::cutovers::latest_for_plan(&pool, "mplan_test")
                .await
                .unwrap()
                .unwrap()
                .state,
            "complete"
        );
    }

    #[tokio::test]
    async fn object_migration_round_trip() {
        use super::object_migrations as om;
        let url = temp_db_url("object-migration");
        let pool = connect(&url).await.unwrap();
        migrate(&pool, &url).await.unwrap();

        om::insert(
            &pool,
            &om::NewObjectMigration {
                id: "objmig_test".into(),
                tenant_id: "global".into(),
                name: "datasets -> ceph".into(),
                source_provider: "aws".into(),
                source_endpoint: "https://s3.us-east-1.amazonaws.com".into(),
                source_region: "us-east-1".into(),
                source_bucket: "training-data".into(),
                source_prefix: Some("models/".into()),
                source_secret_ref: Some("aws-migration-creds".into()),
                dest_provider: "s3-compatible".into(),
                dest_endpoint: "http://rook-ceph-rgw.zyvor:80".into(),
                dest_region: "us-east-1".into(),
                dest_bucket: "ai-datasets".into(),
                dest_secret_ref: Some("rgw-creds".into()),
                secret_namespace: "zyvor-databridge".into(),
                mode: "incremental".into(),
                concurrency: Some(8),
                part_size_mb: Some(16),
            },
        )
        .await
        .unwrap();

        let rec = om::get(&pool, "objmig_test").await.unwrap().unwrap();
        assert_eq!(rec.state, "created");
        assert_eq!(rec.source_bucket, "training-data");
        assert_eq!(rec.source_prefix.as_deref(), Some("models/"));
        assert!(!rec.verified);
        assert_eq!(rec.concurrency, Some(8));
        assert_eq!(rec.part_size_mb, Some(16));

        om::set_totals(&pool, "objmig_test", 42, 1024)
            .await
            .unwrap();
        om::set_started(&pool, "objmig_test").await.unwrap();
        om::set_state(&pool, "objmig_test", "copying")
            .await
            .unwrap();
        om::set_progress(&pool, "objmig_test", 20, 512, 12.5)
            .await
            .unwrap();
        let rec = om::get(&pool, "objmig_test").await.unwrap().unwrap();
        assert_eq!(rec.objects_total, 42);
        assert_eq!(rec.objects_done, 20);
        assert_eq!(rec.state, "copying");
        assert!(rec.throughput_mbps > 12.0);
        assert!(rec.started_at.is_some());

        om::finish(&pool, "objmig_test", "completed", true, None)
            .await
            .unwrap();
        let rec = om::get(&pool, "objmig_test").await.unwrap().unwrap();
        assert_eq!(rec.state, "completed");
        assert!(rec.verified);

        assert_eq!(om::list(&pool, Some("global")).await.unwrap().len(), 1);
        om::delete(&pool, "objmig_test").await.unwrap();
        assert!(om::get(&pool, "objmig_test").await.unwrap().is_none());
    }
}
