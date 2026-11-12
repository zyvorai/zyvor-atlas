// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;
use atlas_driver_k8s::K8sDriver;
use sqlx::AnyPool;
use std::sync::Arc;

use crate::spec::JobSpec;

pub(crate) async fn dispatch_databridge(
    pool: &AnyPool,
    k8s: &Option<Arc<K8sDriver>>,
    spec: JobSpec,
) -> Result<serde_json::Value> {
    match spec {
        JobSpec::SourceDiscover { source_id } => {
            atlas_databridge::pipeline::discover(pool, k8s.as_deref(), &source_id).await
        }

        JobSpec::MigrationAssess { plan_id } => {
            atlas_databridge::pipeline::assess_plan(pool, &plan_id).await
        }

        JobSpec::EdgeDbProvision { plan_id } => {
            atlas_databridge::pipeline::provision_edge(pool, k8s.as_deref(), &plan_id).await
        }

        JobSpec::FullLoad { plan_id } => {
            atlas_databridge::pipeline::full_load(pool, k8s.as_deref(), &plan_id).await
        }
        JobSpec::CdcStart { plan_id } => {
            atlas_databridge::pipeline::start_cdc(pool, k8s.as_deref(), &plan_id).await
        }
        JobSpec::CdcRestart { plan_id } => {
            atlas_databridge::pipeline::restart_cdc(pool, k8s.as_deref(), &plan_id).await
        }
        JobSpec::CdcStop { plan_id } => {
            atlas_databridge::pipeline::stop_cdc(pool, k8s.as_deref(), &plan_id).await
        }
        JobSpec::ValidateRun { plan_id, kind } => {
            atlas_databridge::pipeline::validate(pool, k8s.as_deref(), &plan_id, &kind).await
        }
        JobSpec::Cutover { plan_id } => {
            atlas_databridge::pipeline::cutover(pool, k8s.as_deref(), &plan_id).await
        }
        JobSpec::Rollback { plan_id } => {
            atlas_databridge::pipeline::rollback(pool, k8s.as_deref(), &plan_id).await
        }
        JobSpec::ObjectMigrate { migration_id } => {
            // Clone the Arc into an owned Option before awaiting so the borrow of `k8s`
            // isn't held across the (large) copy future — keeps the future Send-inferable.
            let k8s_owned = k8s.clone();
            atlas_databridge::object::run_migration(pool, k8s_owned, &migration_id)
                .await
                .map(
                    |()| serde_json::json!({ "migration_id": migration_id, "result": "completed" }),
                )
        }

        _ => anyhow::bail!("not a databridge spec"),
    }
}
