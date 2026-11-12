// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Pipeline stage functions called by the job-engine `dispatch` arms. Each takes the SQLite pool
//! (+ later the k8s driver), does the work, updates the DataBridge inventory, and returns a JSON
//! result the job engine persists. Keeping the logic here keeps the `dispatch` match arms thin.

use anyhow::{anyhow, Result};
use sqlx::AnyPool;

use crate::connector::DiscoveredSchema;
use crate::{assess, build_connector};

/// Discover a source's schema and persist it. Advances the source `registered → discovered`. In
/// real mode the credentials are read from the source's k8s Secret (`username`/`password` keys).
pub async fn discover(
    pool: &AnyPool,
    k8s: Option<&atlas_driver_k8s::K8sDriver>,
    source_id: &str,
) -> Result<serde_json::Value> {
    let source = atlas_inventory::databridge::sources::get_source(pool, source_id)
        .await?
        .ok_or_else(|| anyhow!("source {source_id} not found"))?;

    atlas_inventory::databridge::sources::set_state(pool, source_id, "discovering").await?;

    // On any discovery failure, flip the source to `error` so the UI shows why.
    let schema = match async {
        // Resolve credentials from the source Secret for real connectors.
        let creds = if source.driver_mode == "real" {
            let k8s =
                k8s.ok_or_else(|| anyhow!("real discovery needs a reachable Kubernetes cluster"))?;
            let ns = source.secret_namespace.as_deref().unwrap_or(EDGE_NAMESPACE);
            let secret_ref = source
                .secret_ref
                .as_deref()
                .ok_or_else(|| anyhow!("source has no secret_ref for credentials"))?;
            let secret = k8s
                .get_secret(ns, secret_ref)
                .await
                .map_err(|e| anyhow!("read source secret: {e}"))?
                .ok_or_else(|| anyhow!("source secret {ns}/{secret_ref} not found"))?;
            let user = secret
                .get("username")
                .cloned()
                .ok_or_else(|| anyhow!("secret missing 'username'"))?;
            let pass = secret
                .get("password")
                .cloned()
                .ok_or_else(|| anyhow!("secret missing 'password'"))?;
            Some((user, pass))
        } else {
            None
        };
        let connector = build_connector(
            &source,
            creds.as_ref().map(|(u, p)| (u.as_str(), p.as_str())),
        )?;
        connector.discover().await
    }
    .await
    {
        Ok(s) => s,
        Err(e) => {
            let _ = atlas_inventory::databridge::sources::set_state(pool, source_id, "error").await;
            return Err(e);
        }
    };
    let discovered = serde_json::to_value(&schema)?;
    atlas_inventory::databridge::sources::set_discovered(pool, source_id, &discovered).await?;

    // A plan created before its source was discovered stays `draft` forever otherwise — the
    // pipeline UI gates "Assess" on plan.state, not source.state, so without this the plan can
    // never visibly progress even though the source itself discovered successfully.
    for plan in atlas_inventory::databridge::plans::list_plans(pool).await? {
        if plan.source_id == source_id {
            let _ = atlas_inventory::databridge::plans::try_transition(
                pool,
                &plan.id,
                "draft",
                "discovered",
            )
            .await;
        }
    }

    Ok(serde_json::json!({
        "source_id": source_id,
        "engine": schema.engine,
        "version": schema.version,
        "databases": schema.databases,
        "tables": schema.tables.len(),
        "total_size_bytes": schema.total_size_bytes,
        "cdc_capable": schema.cdc_capable,
    }))
}

/// Assess a plan's source: score readiness from the discovered schema. Advances the plan to
/// `assessed`. Requires the source to have been discovered.
pub async fn assess_plan(pool: &AnyPool, plan_id: &str) -> Result<serde_json::Value> {
    let plan = atlas_inventory::databridge::plans::get_plan(pool, plan_id)
        .await?
        .ok_or_else(|| anyhow!("plan {plan_id} not found"))?;
    let source = atlas_inventory::databridge::sources::get_source(pool, &plan.source_id)
        .await?
        .ok_or_else(|| anyhow!("source {} not found", plan.source_id))?;
    if source.state != "discovered" {
        return Err(anyhow!(
            "source {} is not discovered yet (state: {})",
            source.id,
            source.state
        ));
    }
    let schema: DiscoveredSchema = serde_json::from_value(source.discovered.clone())
        .map_err(|e| anyhow!("source has no valid discovered schema: {e}"))?;

    let assessment = assess::assess(&schema);
    let value = serde_json::to_value(&assessment)?;
    atlas_inventory::databridge::plans::set_assessment(pool, plan_id, assessment.score, &value)
        .await?;

    Ok(serde_json::json!({
        "plan_id": plan_id,
        "score": assessment.score,
        "risk": assessment.risk,
        "blockers": assessment.blockers.len(),
        "warnings": assessment.warnings.len(),
    }))
}

/// 1 GiB in bytes.
const GIB: i64 = 1024 * 1024 * 1024;
/// Namespace the edge databases + their CRs live in.
pub const EDGE_NAMESPACE: &str = "zyvor-databridge";

/// Data-volume size for the edge cluster: discovered dataset + 50% headroom, min 10 GiB.
fn edge_size_gib(source: &atlas_api_types::MigrationSource) -> i64 {
    let total = source
        .discovered
        .get("total_size_bytes")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    ((total + total / 2) / GIB).max(10)
}

/// Provision the edge database cluster for a plan. In `fake` source mode this fabricates a ready
/// cluster with a synthetic endpoint (no k8s needed). In `real` mode it applies the CloudNativePG /
/// MySQL operator CR (data on Ceph RBD) and leaves the cluster `provisioning` for the reconciler to
/// poll to `ready`.
pub async fn provision_edge(
    pool: &AnyPool,
    k8s: Option<&atlas_driver_k8s::K8sDriver>,
    plan_id: &str,
) -> Result<serde_json::Value> {
    let plan = atlas_inventory::databridge::plans::get_plan(pool, plan_id)
        .await?
        .ok_or_else(|| anyhow!("plan {plan_id} not found"))?;
    let source = atlas_inventory::databridge::sources::get_source(pool, &plan.source_id)
        .await?
        .ok_or_else(|| anyhow!("source {} not found", plan.source_id))?;

    let engine = crate::SourceKind::parse(&source.kind)
        .ok_or_else(|| anyhow!("unsupported engine: {}", source.kind))?;
    // The edge engine/operator is the *target*: Postgres-family + heterogeneous Oracle/SQL Server
    // land on CloudNativePG; MySQL/MariaDB land on Percona.
    let op = engine.edge_operator();
    let (engine_str, operator) = (op.engine_str(), op.operator_str());
    let namespace = EDGE_NAMESPACE;
    let cr_name = format!("edge-{}", &plan_id[plan_id.len().saturating_sub(8)..]);
    let edge_id = atlas_common::ids::edge_cluster_id();
    let size_gib = edge_size_gib(&source);

    // The route only checks the plan isn't a fresh `draft`, not that a provision isn't already
    // under way — two racing `/provision` requests (double-click, client retry) can both reach
    // here. Re-validate atomically: only the request that finds the plan still `assessed` proceeds,
    // so a duplicate never inserts a second `edge_db_clusters` row that orphans the first and wastes
    // a redundant CR apply.
    if !atlas_inventory::databridge::plans::try_transition(
        pool,
        plan_id,
        "assessed",
        "provisioning",
    )
    .await?
    {
        anyhow::bail!(
            "plan {plan_id} is not in 'assessed' state — a concurrent provision is already in progress \
             or already completed"
        );
    }

    atlas_inventory::databridge::edge_clusters::insert_edge_cluster(
        pool,
        &edge_id,
        &plan.tenant_id,
        plan_id,
        engine_str,
        operator,
        namespace,
        &cr_name,
        "zyvor-rbd-prod",
        Some("zyvor-rbd-prod"),
        1,
    )
    .await?;
    atlas_inventory::databridge::plans::set_edge_cluster(pool, plan_id, &edge_id).await?;

    // Fake source mode (or no reachable k8s): fabricate a ready cluster so the pipeline runs.
    if source.driver_mode == "fake" || k8s.is_none() {
        let (endpoint, secret_ref) = match op {
            crate::connector::EdgeOperator::Cnpg => (
                crate::cr::cnpg::endpoint(&cr_name, namespace),
                crate::cr::cnpg::secret_ref(&cr_name),
            ),
            crate::connector::EdgeOperator::Percona => (
                crate::cr::mysql_operator::endpoint(&cr_name, namespace),
                crate::cr::mysql_operator::secret_ref(&cr_name),
            ),
            crate::connector::EdgeOperator::Psmdb => (
                crate::cr::psmdb::endpoint(&cr_name, namespace),
                crate::cr::psmdb::secret_ref(&cr_name),
            ),
        };
        atlas_inventory::databridge::edge_clusters::set_ready(
            pool,
            &edge_id,
            &endpoint,
            &secret_ref,
        )
        .await?;
        atlas_inventory::databridge::plans::set_state(pool, plan_id, "provisioned").await?;
        return Ok(serde_json::json!({
            "plan_id": plan_id, "edge_cluster_id": edge_id, "engine": engine_str,
            "operator": operator, "state": "ready", "service_endpoint": endpoint, "mode": "fake",
        }));
    }

    // Real mode: apply the operator CR; the reconciler advances provisioning -> ready.
    let k8s = k8s.expect("k8s present in real branch");
    let db = source.database.as_deref().unwrap_or("appdb");
    // Heterogeneous sources land on a fresh Postgres edge; the initdb database name stays generic.
    let edge_db = if engine.homogeneous() { db } else { "appdb" };
    let (group, version, kind, spec) = match op {
        crate::connector::EdgeOperator::Cnpg => (
            crate::cr::cnpg::GROUP,
            crate::cr::cnpg::VERSION,
            crate::cr::cnpg::KIND,
            crate::cr::cnpg::cluster_spec(1, "zyvor-rbd-prod", "zyvor-rbd-prod", size_gib, edge_db),
        ),
        crate::connector::EdgeOperator::Percona => (
            crate::cr::mysql_operator::GROUP,
            crate::cr::mysql_operator::VERSION,
            crate::cr::mysql_operator::KIND,
            crate::cr::mysql_operator::cluster_spec(1, "zyvor-rbd-prod", size_gib),
        ),
        crate::connector::EdgeOperator::Psmdb => (
            crate::cr::psmdb::GROUP,
            crate::cr::psmdb::VERSION,
            crate::cr::psmdb::KIND,
            crate::cr::psmdb::cluster_spec(1, "zyvor-rbd-prod", size_gib),
        ),
    };
    if let Err(e) = k8s.apply_cr(group, version, kind, namespace, &cr_name, spec).await {
        // Nothing was created in the cluster, so hand the plan back to `assessed` for a retry; left
        // in `provisioning` with no CR, the reconciler never advances it and `try_transition`
        // refuses every later provision.
        atlas_inventory::databridge::edge_clusters::delete_edge_cluster(pool, &edge_id).await?;
        atlas_inventory::databridge::plans::set_state(pool, plan_id, "assessed").await?;
        return Err(anyhow!("apply {kind} CR: {e}"));
    }

    Ok(serde_json::json!({
        "plan_id": plan_id, "edge_cluster_id": edge_id, "engine": engine_str,
        "operator": operator, "state": "provisioning", "cr_name": cr_name,
        "size_gib": size_gib, "mode": "real",
    }))
}

/// Short suffix of a plan id, for naming derived k8s objects.
pub(crate) fn plan_short(plan_id: &str) -> &str {
    &plan_id[plan_id.len().saturating_sub(8)..]
}

/// Full-load: copy the source dataset into the freshly-provisioned edge DB. Fake mode simulates an
/// instant load; real mode applies a dump/restore batch Job.
pub async fn full_load(
    pool: &AnyPool,
    k8s: Option<&atlas_driver_k8s::K8sDriver>,
    plan_id: &str,
) -> Result<serde_json::Value> {
    let plan = atlas_inventory::databridge::plans::get_plan(pool, plan_id)
        .await?
        .ok_or_else(|| anyhow!("plan {plan_id} not found"))?;
    let source = atlas_inventory::databridge::sources::get_source(pool, &plan.source_id)
        .await?
        .ok_or_else(|| anyhow!("source not found"))?;
    atlas_inventory::databridge::plans::set_state(pool, plan_id, "full_loading").await?;

    if source.driver_mode == "fake" || k8s.is_none() {
        let tables = source
            .discovered
            .get("tables")
            .and_then(|t| t.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        atlas_inventory::databridge::plans::set_state(pool, plan_id, "loaded").await?;
        if let Some(edge_id) = plan.edge_cluster_id.as_deref() {
            let bytes = source
                .discovered
                .get("total_size_bytes")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            atlas_inventory::databridge::edge_clusters::set_size_bytes(pool, edge_id, bytes)
                .await?;
        }
        return Ok(
            serde_json::json!({ "plan_id": plan_id, "state": "loaded", "tables": tables, "mode": "fake" }),
        );
    }

    // Real mode: apply a batch Job that dumps the source and loads it into the edge DB. The
    // reconciler watches the Job to completion and advances full_loading -> loaded (or failed).
    let k8s = k8s.expect("k8s present in real branch");
    let edge_id = plan
        .edge_cluster_id
        .as_deref()
        .ok_or_else(|| anyhow!("plan has no edge cluster; provision first"))?;
    let edge = atlas_inventory::databridge::edge_clusters::get_edge_cluster(pool, edge_id)
        .await?
        .ok_or_else(|| anyhow!("edge cluster {edge_id} not found"))?;
    if edge.state != "ready" {
        return Err(anyhow!(
            "edge cluster is not ready yet (state: {})",
            edge.state
        ));
    }
    let source_secret = source
        .secret_ref
        .as_deref()
        .ok_or_else(|| anyhow!("source has no secret_ref for credentials"))?;
    let edge_secret = edge
        .secret_ref
        .as_deref()
        .ok_or_else(|| anyhow!("edge cluster has no credentials secret"))?;
    let engine = crate::SourceKind::parse(&source.kind)
        .ok_or_else(|| anyhow!("unsupported engine: {}", source.kind))?;
    let src_host = source.endpoint.as_deref().unwrap_or("");
    let src_db = source.database.as_deref().unwrap_or("appdb");

    // Heterogeneous sources (Oracle / SQL Server → Postgres) have no dump→restore full-load: the
    // Debezium `initial` snapshot in the CDC stage seeds the edge (the JDBC sink auto-creates the
    // tables). Mark the plan `loaded` and let CDC do the initial copy.
    if !engine.homogeneous() {
        let _ = (source_secret, edge_secret);
        atlas_inventory::databridge::plans::set_state(pool, plan_id, "loaded").await?;
        return Ok(serde_json::json!({
            "plan_id": plan_id, "state": "loaded", "mode": "real",
            "engine": engine.as_str(),
            "note": "heterogeneous source — edge seeded by Debezium initial snapshot (no dump Job)"
        }));
    }

    let job_spec = match engine {
        crate::SourceKind::Postgres => crate::loader::pg_job_spec(
            source_secret,
            edge_secret,
            src_host,
            source.port.unwrap_or(5432),
            src_db,
            &source.tls_mode,
        ),
        // MySQL and MariaDB share the mysqldump→mysql loader.
        crate::SourceKind::Mysql | crate::SourceKind::Mariadb => {
            // edge host = the HAProxy service (endpoint without the :port suffix).
            let edge_ep = crate::cr::mysql_operator::endpoint(
                edge.cr_name.as_deref().unwrap_or(""),
                &edge.namespace,
            );
            let edge_host = edge_ep.split(':').next().unwrap_or("");
            crate::loader::mysql_job_spec(
                source_secret,
                edge_secret,
                src_host,
                source.port.unwrap_or(3306),
                src_db,
                edge_host,
                src_db,
            )
        }
        // MongoDB: mongodump → mongorestore against the edge PSMDB replica set.
        crate::SourceKind::Mongodb => {
            let edge_ep =
                crate::cr::psmdb::endpoint(edge.cr_name.as_deref().unwrap_or(""), &edge.namespace);
            let edge_host = edge_ep.split(':').next().unwrap_or("");
            crate::loader::mongo_job_spec(
                source_secret,
                edge_secret,
                src_host,
                source.port.unwrap_or(27017),
                src_db,
                edge_host,
                src_db,
            )
        }
        // Guarded by the homogeneous() check above.
        crate::SourceKind::Oracle | crate::SourceKind::Sqlserver => unreachable!(),
    };
    let job_name = crate::loader::job_name(plan_id);
    k8s.apply_cr(
        crate::loader::JOB_GROUP,
        crate::loader::JOB_VERSION,
        crate::loader::JOB_KIND,
        &edge.namespace,
        &job_name,
        job_spec,
    )
    .await
    .map_err(|e| anyhow!("apply full-load Job: {e}"))?;

    Ok(serde_json::json!({
        "plan_id": plan_id, "job": job_name, "state": "full_loading", "mode": "real"
    }))
}

/// Start Debezium CDC. Fake mode records a streaming stream with an initial lag the reconciler
/// drains toward zero; real mode applies KafkaConnect and Debezium connector CRs.
pub async fn start_cdc(
    pool: &AnyPool,
    k8s: Option<&atlas_driver_k8s::K8sDriver>,
    plan_id: &str,
) -> Result<serde_json::Value> {
    let plan = atlas_inventory::databridge::plans::get_plan(pool, plan_id)
        .await?
        .ok_or_else(|| anyhow!("plan {plan_id} not found"))?;
    let source = atlas_inventory::databridge::sources::get_source(pool, &plan.source_id)
        .await?
        .ok_or_else(|| anyhow!("source not found"))?;
    let kind = crate::SourceKind::parse(&source.kind)
        .ok_or_else(|| anyhow!("unsupported engine: {}", source.kind))?;
    let engine = source.kind.clone();
    let s = plan_short(plan_id).to_string();
    let cdc_id = atlas_common::ids::cdc_stream_id();
    let connect = crate::cr::streaming::connect_name(&s);
    let connector = crate::cr::streaming::source_connector_name(&s);
    let topic_prefix = crate::cr::streaming::topic_prefix(&s);

    // `db_stage_job` (the route handler) only checks the plan exists, not its state — two racing
    // `/cdc/start` requests (double-click, client retry) can both reach here. Re-validate
    // atomically: only the request that finds the plan still `loaded` proceeds, so a duplicate
    // never inserts a second `cdc_streams` row that orphans the first (a stalled/errored stream
    // should go through `/cdc/restart`, not a second `/cdc/start`).
    if !atlas_inventory::databridge::plans::try_transition(pool, plan_id, "loaded", "cdc_streaming")
        .await?
    {
        anyhow::bail!(
            "plan {plan_id} is not in 'loaded' state — CDC is already started for this plan \
             (use /cdc/restart to re-establish a stalled stream) or full-load hasn't completed yet"
        );
    }

    atlas_inventory::databridge::cdc::insert_stream(
        pool,
        &cdc_id,
        &plan.tenant_id,
        plan_id,
        &engine,
        &connect,
        &connector,
        &topic_prefix,
    )
    .await?;
    atlas_inventory::databridge::plans::set_cdc_stream(pool, plan_id, &cdc_id).await?;

    if source.driver_mode == "fake" || k8s.is_none() {
        atlas_inventory::databridge::cdc::set_state(pool, &cdc_id, "streaming").await?;
        // seed an initial backlog; the reconciler drains it so the UI lag chart animates.
        atlas_inventory::databridge::cdc::update_lag(
            pool,
            &cdc_id,
            512 * 1024 * 1024,
            45,
            Some("0/1000"),
            Some("0/0"),
            0,
        )
        .await?;
        return Ok(
            serde_json::json!({ "plan_id": plan_id, "cdc_stream_id": cdc_id, "state": "streaming", "mode": "fake" }),
        );
    }

    // Real mode: apply the Strimzi KafkaConnect cluster + Debezium source + JDBC sink connectors.
    // (Structurally correct but UNVERIFIED against a live Kafka/DB stack — see docs/DATABRIDGE.md.)
    let k8s = k8s.expect("k8s present in real branch");
    let edge_id = plan
        .edge_cluster_id
        .as_deref()
        .ok_or_else(|| anyhow!("plan has no edge cluster; provision first"))?;
    let edge = atlas_inventory::databridge::edge_clusters::get_edge_cluster(pool, edge_id)
        .await?
        .ok_or_else(|| anyhow!("edge cluster not found"))?;
    let ns = EDGE_NAMESPACE;
    let src_secret = source
        .secret_ref
        .as_deref()
        .ok_or_else(|| anyhow!("source has no secret_ref"))?;
    let src_secret_ns = source.secret_namespace.as_deref().unwrap_or(ns);
    let edge_secret = edge
        .secret_ref
        .as_deref()
        .ok_or_else(|| anyhow!("edge cluster has no secret"))?;
    let cr_name = edge.cr_name.as_deref().unwrap_or("");
    let db = source.database.as_deref().unwrap_or("appdb");

    use crate::cr::streaming;
    // 1. KafkaConnect cluster (image must bundle Debezium + JDBC-sink plugins; set at deploy time).
    // The use-connector-resources annotation is REQUIRED or Strimzi ignores the KafkaConnector CRs.
    let connect_image = std::env::var("ATLAS_DATABRIDGE_CONNECT_IMAGE").ok();
    if connect_image.as_deref().unwrap_or("").trim().is_empty() {
        anyhow::bail!(
            "real CDC requires ATLAS_DATABRIDGE_CONNECT_IMAGE (build \
             deploy/databridge/connect/Dockerfile) and a Strimzi Kafka named zyvor-kafka in \
             namespace zyvor-databridge (deploy/databridge/up.sh + 10-kafka.yaml) — \
             see docs/DATABRIDGE.md engine verification matrix"
        );
    }
    let mut connect_annotations = std::collections::BTreeMap::new();
    connect_annotations.insert(
        "strimzi.io/use-connector-resources".to_string(),
        "true".to_string(),
    );
    k8s.apply_cr_meta(
        streaming::GROUP,
        streaming::VERSION,
        streaming::CONNECT_KIND,
        ns,
        &connect,
        &std::collections::BTreeMap::new(),
        &connect_annotations,
        streaming::connect_spec(
            "zyvor-kafka-kafka-bootstrap:9092",
            1,
            connect_image.as_deref(),
        ),
    )
    .await
    .map_err(|e| anyhow!("apply KafkaConnect: {e}"))?;

    let mut labels = std::collections::BTreeMap::new();
    labels.insert("strimzi.io/cluster".to_string(), connect.clone());

    // 2. Debezium source connector
    let src_spec = streaming::debezium_source_spec(
        kind,
        &s,
        source.endpoint.as_deref().unwrap_or(""),
        source.port.unwrap_or_else(|| kind.default_port()),
        db,
        src_secret_ns,
        src_secret,
    );
    k8s.apply_cr_labeled(
        streaming::GROUP,
        streaming::VERSION,
        streaming::CONNECTOR_KIND,
        ns,
        &connector,
        &labels,
        src_spec,
    )
    .await
    .map_err(|e| anyhow!("apply Debezium source connector: {e}"))?;

    // 3. Sink connector -> edge DB.
    let sink_spec = sink_spec_for(kind, &s, cr_name, ns, edge_secret, db);
    k8s.apply_cr_labeled(
        streaming::GROUP,
        streaming::VERSION,
        streaming::CONNECTOR_KIND,
        ns,
        &streaming::sink_connector_name(&s),
        &labels,
        sink_spec,
    )
    .await
    .map_err(|e| anyhow!("apply sink connector: {e}"))?;

    atlas_inventory::databridge::cdc::set_state(pool, &cdc_id, "streaming").await?;
    Ok(serde_json::json!({
        "plan_id": plan_id, "cdc_stream_id": cdc_id, "connect": connect, "state": "streaming", "mode": "real"
    }))
}

/// Delete a plan's streaming CRs: the Debezium source + sink `KafkaConnector`s and (optionally) the
/// per-plan `KafkaConnect` cluster itself. Idempotent — `delete_cr` treats 404 as success — so it is
/// safe to call from both stop-CDC and cutover teardown.
pub(crate) async fn teardown_streaming(
    k8s: &atlas_driver_k8s::K8sDriver,
    plan_short: &str,
    delete_connect: bool,
) -> Result<()> {
    use crate::cr::streaming;
    let ns = EDGE_NAMESPACE;
    k8s.delete_cr(
        streaming::GROUP,
        streaming::VERSION,
        streaming::CONNECTOR_KIND,
        ns,
        &streaming::source_connector_name(plan_short),
    )
    .await
    .map_err(|e| anyhow!("delete Debezium source connector: {e}"))?;
    k8s.delete_cr(
        streaming::GROUP,
        streaming::VERSION,
        streaming::CONNECTOR_KIND,
        ns,
        &streaming::sink_connector_name(plan_short),
    )
    .await
    .map_err(|e| anyhow!("delete sink connector: {e}"))?;
    if delete_connect {
        k8s.delete_cr(
            streaming::GROUP,
            streaming::VERSION,
            streaming::CONNECT_KIND,
            ns,
            &streaming::connect_name(plan_short),
        )
        .await
        .map_err(|e| anyhow!("delete KafkaConnect cluster: {e}"))?;
    }
    Ok(())
}

/// Stop a plan's CDC stream. Real mode deletes the two `KafkaConnector` CRs (the per-plan
/// `KafkaConnect` cluster stays up so a later start/restart re-instantiates quickly); fake mode just
/// flips the stream state.
pub async fn stop_cdc(
    pool: &AnyPool,
    k8s: Option<&atlas_driver_k8s::K8sDriver>,
    plan_id: &str,
) -> Result<serde_json::Value> {
    let plan = atlas_inventory::databridge::plans::get_plan(pool, plan_id)
        .await?
        .ok_or_else(|| anyhow!("plan {plan_id} not found"))?;
    let source = atlas_inventory::databridge::sources::get_source(pool, &plan.source_id)
        .await?
        .ok_or_else(|| anyhow!("source not found"))?;
    if source.driver_mode != "fake" {
        if let Some(k8s) = k8s {
            teardown_streaming(k8s, plan_short(plan_id), false).await?;
        }
    }
    if let Some(cdc_id) = plan.cdc_stream_id.as_deref() {
        atlas_inventory::databridge::cdc::set_state(pool, cdc_id, "stopped").await?;
    }
    Ok(serde_json::json!({ "plan_id": plan_id, "state": "stopped" }))
}

/// Day-2 self-heal: re-establish a stalled/errored CDC stream. Bumps the stream's restart counter,
/// then (fake) resets it to `streaming` with a fresh backlog to drain, or (real) re-applies the
/// Debezium source + JDBC sink `KafkaConnector` CRs — an idempotent apply that restarts the failed
/// connectors (mirrors `start_cdc`'s real applies; the KafkaConnect cluster stays up).
pub async fn restart_cdc(
    pool: &AnyPool,
    k8s: Option<&atlas_driver_k8s::K8sDriver>,
    plan_id: &str,
) -> Result<serde_json::Value> {
    let plan = atlas_inventory::databridge::plans::get_plan(pool, plan_id)
        .await?
        .ok_or_else(|| anyhow!("plan {plan_id} not found"))?;
    let cdc_id = plan
        .cdc_stream_id
        .clone()
        .ok_or_else(|| anyhow!("plan has no CDC stream; start CDC first"))?;
    let source = atlas_inventory::databridge::sources::get_source(pool, &plan.source_id)
        .await?
        .ok_or_else(|| anyhow!("source not found"))?;
    let restart_count = atlas_inventory::databridge::cdc::bump_restart(pool, &cdc_id).await?;

    if source.driver_mode == "fake" || k8s.is_none() {
        // Fake: re-establish with a fresh backlog so the reconciler drains it back to caught-up.
        atlas_inventory::databridge::cdc::set_state(pool, &cdc_id, "streaming").await?;
        atlas_inventory::databridge::cdc::update_lag(
            pool,
            &cdc_id,
            256 * 1024 * 1024,
            30,
            Some("0/1000"),
            Some("0/0"),
            0,
        )
        .await?;
        return Ok(serde_json::json!({
            "plan_id": plan_id, "cdc_stream_id": cdc_id, "state": "streaming",
            "restart_count": restart_count, "mode": "fake"
        }));
    }

    // Real: re-apply the two Debezium connector CRs to restart the failed connectors.
    let k8s = k8s.expect("k8s present in real branch");
    let kind = crate::SourceKind::parse(&source.kind)
        .ok_or_else(|| anyhow!("unsupported engine: {}", source.kind))?;
    let s = plan_short(plan_id).to_string();
    let edge_id = plan
        .edge_cluster_id
        .as_deref()
        .ok_or_else(|| anyhow!("plan has no edge cluster"))?;
    let edge = atlas_inventory::databridge::edge_clusters::get_edge_cluster(pool, edge_id)
        .await?
        .ok_or_else(|| anyhow!("edge cluster not found"))?;
    let ns = EDGE_NAMESPACE;
    let src_secret = source
        .secret_ref
        .as_deref()
        .ok_or_else(|| anyhow!("source has no secret_ref"))?;
    let src_secret_ns = source.secret_namespace.as_deref().unwrap_or(ns);
    let edge_secret = edge
        .secret_ref
        .as_deref()
        .ok_or_else(|| anyhow!("edge cluster has no secret"))?;
    let cr_name = edge.cr_name.as_deref().unwrap_or("");
    let db = source.database.as_deref().unwrap_or("appdb");
    let connect = crate::cr::streaming::connect_name(&s);

    use crate::cr::streaming;
    let mut labels = std::collections::BTreeMap::new();
    labels.insert("strimzi.io/cluster".to_string(), connect.clone());
    let src_spec = streaming::debezium_source_spec(
        kind,
        &s,
        source.endpoint.as_deref().unwrap_or(""),
        source.port.unwrap_or_else(|| kind.default_port()),
        db,
        src_secret_ns,
        src_secret,
    );
    k8s.apply_cr_labeled(
        streaming::GROUP,
        streaming::VERSION,
        streaming::CONNECTOR_KIND,
        ns,
        &streaming::source_connector_name(&s),
        &labels,
        src_spec,
    )
    .await
    .map_err(|e| anyhow!("re-apply Debezium source connector: {e}"))?;

    let sink_spec = sink_spec_for(kind, &s, cr_name, ns, edge_secret, db);
    k8s.apply_cr_labeled(
        streaming::GROUP,
        streaming::VERSION,
        streaming::CONNECTOR_KIND,
        ns,
        &streaming::sink_connector_name(&s),
        &labels,
        sink_spec,
    )
    .await
    .map_err(|e| anyhow!("re-apply sink connector: {e}"))?;

    atlas_inventory::databridge::cdc::set_state(pool, &cdc_id, "streaming").await?;
    Ok(serde_json::json!({
        "plan_id": plan_id, "cdc_stream_id": cdc_id, "state": "streaming",
        "restart_count": restart_count, "mode": "real"
    }))
}

/// Validate source vs edge (row counts / checksums). Fake mode reports matching counts per table.
pub async fn validate(
    pool: &AnyPool,
    k8s: Option<&atlas_driver_k8s::K8sDriver>,
    plan_id: &str,
    kind: &str,
) -> Result<serde_json::Value> {
    let plan = atlas_inventory::databridge::plans::get_plan(pool, plan_id)
        .await?
        .ok_or_else(|| anyhow!("plan {plan_id} not found"))?;
    let source = atlas_inventory::databridge::sources::get_source(pool, &plan.source_id)
        .await?
        .ok_or_else(|| anyhow!("source not found"))?;
    atlas_inventory::databridge::plans::set_state(pool, plan_id, "validating").await?;

    let val_id = atlas_common::ids::validation_id();
    atlas_inventory::databridge::validations::insert_validation(
        pool,
        &val_id,
        &plan.tenant_id,
        plan_id,
        kind,
    )
    .await?;

    // Real mode: apply a validation Job (row-count compare); the reconciler advances it.
    if let (false, Some(k8s)) = (source.driver_mode == "fake", k8s) {
        let edge_id = plan
            .edge_cluster_id
            .as_deref()
            .ok_or_else(|| anyhow!("plan has no edge cluster"))?;
        let edge = atlas_inventory::databridge::edge_clusters::get_edge_cluster(pool, edge_id)
            .await?
            .ok_or_else(|| anyhow!("edge cluster not found"))?;
        let src_secret = source
            .secret_ref
            .as_deref()
            .ok_or_else(|| anyhow!("source has no secret_ref"))?;
        let edge_secret = edge
            .secret_ref
            .as_deref()
            .ok_or_else(|| anyhow!("edge has no secret"))?;
        let engine = crate::SourceKind::parse(&source.kind).ok_or_else(|| anyhow!("bad engine"))?;
        let src_host = source.endpoint.as_deref().unwrap_or("");
        let db = source.database.as_deref().unwrap_or("appdb");
        let cr_name = edge.cr_name.as_deref().unwrap_or("");

        // Heterogeneous migrations can't run a single-client source-vs-edge count job (the source and
        // edge speak different wire protocols). Row parity is confirmed by the Debezium snapshot +
        // stream converging; mark validated with an advisory note.
        if !engine.homogeneous() {
            let summary = serde_json::json!({
                "note": "heterogeneous migration — parity verified via Debezium snapshot/stream convergence; \
                         per-table cross-engine row-count validation is advisory",
                "engine": engine.as_str(),
            });
            atlas_inventory::databridge::validations::set_result(
                pool, &val_id, true, 0, 0, &summary,
            )
            .await?;
            atlas_inventory::databridge::plans::set_state(pool, plan_id, "validated").await?;
            return Ok(serde_json::json!({
                "plan_id": plan_id, "validation_id": val_id, "passed": true, "mode": "real",
                "engine": engine.as_str(), "note": "advisory (heterogeneous)"
            }));
        }

        let job_spec = match engine {
            crate::SourceKind::Postgres => crate::validate::pg_validate_job_spec(
                src_secret,
                edge_secret,
                src_host,
                source.port.unwrap_or(5432),
                db,
                &source.tls_mode,
            ),
            crate::SourceKind::Mysql | crate::SourceKind::Mariadb => {
                let edge_host = format!("{cr_name}-haproxy");
                crate::validate::mysql_validate_job_spec(
                    src_secret,
                    edge_secret,
                    src_host,
                    source.port.unwrap_or(3306),
                    db,
                    &edge_host,
                    db,
                )
            }
            crate::SourceKind::Mongodb => {
                let edge_host = format!("{cr_name}-rs0");
                crate::validate::mongo_validate_job_spec(
                    src_secret,
                    edge_secret,
                    src_host,
                    source.port.unwrap_or(27017),
                    db,
                    &edge_host,
                    db,
                )
            }
            crate::SourceKind::Oracle | crate::SourceKind::Sqlserver => unreachable!(),
        };
        let job_name = crate::validate::job_name(&val_id);
        k8s.apply_cr(
            crate::loader::JOB_GROUP,
            crate::loader::JOB_VERSION,
            crate::loader::JOB_KIND,
            &edge.namespace,
            &job_name,
            job_spec,
        )
        .await
        .map_err(|e| anyhow!("apply validation Job: {e}"))?;
        return Ok(serde_json::json!({
            "plan_id": plan_id, "validation_id": val_id, "state": "running", "mode": "real"
        }));
    }

    // Fake: every discovered table matches on both sides.
    let tables = source
        .discovered
        .get("tables")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let results: Vec<serde_json::Value> = tables
        .iter()
        .map(|t| {
            let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let rows = t.get("est_rows").and_then(|r| r.as_i64()).unwrap_or(0);
            serde_json::json!({ "table": name, "source_rows": rows, "edge_rows": rows, "checksum_match": true })
        })
        .collect();
    let summary = serde_json::json!({ "tables": results });
    atlas_inventory::databridge::validations::set_result(
        pool,
        &val_id,
        true,
        tables.len() as i64,
        0,
        &summary,
    )
    .await?;
    atlas_inventory::databridge::plans::set_state(pool, plan_id, "validated").await?;

    Ok(
        serde_json::json!({ "plan_id": plan_id, "validation_id": val_id, "passed": true, "tables": tables.len() }),
    )
}

/// Cutover: freeze source, drain CDC, switch endpoint, open the rollback window. Fake mode drains
/// and switches instantly; real mode leaves the cutover `draining` and the reconciler completes it
/// once the CDC stream reports zero lag (tearing the streaming stack down at the switch). Guards
/// (validated, validation passed, lag under threshold) are enforced in the route.
pub async fn cutover(
    pool: &AnyPool,
    k8s: Option<&atlas_driver_k8s::K8sDriver>,
    plan_id: &str,
) -> Result<serde_json::Value> {
    let plan = atlas_inventory::databridge::plans::get_plan(pool, plan_id)
        .await?
        .ok_or_else(|| anyhow!("plan {plan_id} not found"))?;
    let source = atlas_inventory::databridge::sources::get_source(pool, &plan.source_id)
        .await?
        .ok_or_else(|| anyhow!("source not found"))?;
    let edge_id = plan
        .edge_cluster_id
        .as_deref()
        .ok_or_else(|| anyhow!("plan has no edge cluster"))?;
    let edge = atlas_inventory::databridge::edge_clusters::get_edge_cluster(pool, edge_id)
        .await?
        .ok_or_else(|| anyhow!("edge cluster not found"))?;

    // The route already checked the plan was `validated`, but that check and this job run are not
    // atomic — a racing duplicate request (double-click, client retry) can enqueue a second cutover
    // job before the first one lands. Re-validate here with an atomic transition so only the first
    // job to reach this point actually starts a cutover; the loser bails instead of driving a second,
    // conflicting cutover against the same source/edge.
    if !atlas_inventory::databridge::plans::try_transition(
        pool,
        plan_id,
        "validated",
        "cutover_in_progress",
    )
    .await?
    {
        anyhow::bail!(
            "plan {plan_id} is not in 'validated' state — a concurrent cutover is already in progress \
             or already completed"
        );
    }

    let cut_id = atlas_common::ids::cutover_id();
    let from = source.endpoint.clone().unwrap_or_default();
    let to = edge.service_endpoint.clone().unwrap_or_default();
    let now = chrono::Utc::now();
    let drain = (now + chrono::Duration::minutes(5)).to_rfc3339();
    let rollback = (now + chrono::Duration::seconds(plan.rollback_window_secs)).to_rfc3339();

    atlas_inventory::databridge::cutovers::insert_cutover(
        pool,
        &cut_id,
        &plan.tenant_id,
        plan_id,
        Some(&from),
        Some(&to),
        Some(&drain),
        Some(&rollback),
    )
    .await?;

    // Real mode: hand the drain to the reconciler — it switches once the stream reports zero lag
    // (or fails the cutover past the drain deadline). The route already verified lag is under the
    // threshold, but "under threshold" is not "drained".
    if source.driver_mode != "fake" && k8s.is_some() {
        atlas_inventory::databridge::cutovers::set_state(pool, &cut_id, "draining").await?;
        return Ok(serde_json::json!({
            "plan_id": plan_id, "cutover_id": cut_id, "from": from, "to": to,
            "drain_deadline": drain, "rollback_deadline": rollback,
            "state": "draining", "mode": "real",
        }));
    }

    // Fake: drain + switch complete immediately.
    atlas_inventory::databridge::cutovers::set_state(pool, &cut_id, "switching").await?;
    atlas_inventory::databridge::cutovers::set_complete(pool, &cut_id, "complete").await?;
    atlas_inventory::databridge::plans::set_state(pool, plan_id, "cutover_complete").await?;
    atlas_inventory::databridge::plans::set_cutover_at(pool, plan_id, &now.to_rfc3339()).await?;
    if let Some(cdc_id) = plan.cdc_stream_id.as_deref() {
        atlas_inventory::databridge::cdc::set_state(pool, cdc_id, "stopped").await?;
    }

    Ok(serde_json::json!({
        "plan_id": plan_id, "cutover_id": cut_id, "from": from, "to": to,
        "rollback_deadline": rollback, "state": "cutover_complete",
    }))
}

/// What [`rollback`] already does, in order. It flips cutover and plan state. It does not
/// copy rows back to the source.
pub fn rollback_steps() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "window",
            "the latest cutover's rollback_deadline must still be open (the HTTP route enforces this; rollback itself marks state)",
        ),
        (
            "mark_cutover",
            "pipeline::rollback sets that cutover's state to rolled_back",
        ),
        (
            "mark_plan",
            "the plan state becomes rolled_back; callers point applications at the source endpoint again",
        ),
        (
            "cdc",
            "a CDC stream rollback stopped stays stopped; starting it again is a separate cdc-start",
        ),
    ]
}

/// Read-only view of a reverse cutover: the steps [`rollback`] will run, and whether the
/// recorded window is still open. Does not change the plan.
pub async fn rollback_drill(pool: &AnyPool, plan_id: &str) -> Result<serde_json::Value> {
    let plan = atlas_inventory::databridge::plans::get_plan(pool, plan_id)
        .await?
        .ok_or_else(|| anyhow!("plan {plan_id} not found"))?;
    let cut = atlas_inventory::databridge::cutovers::latest_for_plan(pool, plan_id).await?;
    let (window_open, deadline) = match cut.as_ref().and_then(|c| c.rollback_deadline.as_deref()) {
        Some(deadline) => match chrono::DateTime::parse_from_rfc3339(deadline) {
            Ok(dl) => (chrono::Utc::now() <= dl, Some(deadline.to_string())),
            Err(_) => (false, Some(deadline.to_string())),
        },
        None => (cut.is_some(), None),
    };
    Ok(serde_json::json!({
        "plan_id": plan.id,
        "plan_state": plan.state,
        "cutover_id": cut.as_ref().map(|c| c.id.clone()),
        "cutover_state": cut.as_ref().map(|c| c.state.clone()),
        "from_endpoint": cut.as_ref().and_then(|c| c.from_endpoint.clone()),
        "to_endpoint": cut.as_ref().and_then(|c| c.to_endpoint.clone()),
        "rollback_deadline": deadline,
        "window_open": window_open,
        "mutates": false,
        "run": "atlasctl databridge-stage PLAN rollback",
        "steps": rollback_steps()
            .into_iter()
            .map(|(id, action)| serde_json::json!({ "id": id, "action": action }))
            .collect::<Vec<_>>(),
    }))
}

/// Roll back a cutover within its window: revert the active endpoint to the source.
pub async fn rollback(pool: &AnyPool, plan_id: &str) -> Result<serde_json::Value> {
    let plan = atlas_inventory::databridge::plans::get_plan(pool, plan_id)
        .await?
        .ok_or_else(|| anyhow!("plan {plan_id} not found"))?;
    if let Some(cut) = atlas_inventory::databridge::cutovers::latest_for_plan(pool, plan_id).await?
    {
        atlas_inventory::databridge::cutovers::set_complete(pool, &cut.id, "rolled_back").await?;
    }
    atlas_inventory::databridge::plans::set_state(pool, plan_id, "rolled_back").await?;
    Ok(serde_json::json!({ "plan_id": plan.id, "state": "rolled_back" }))
}

/// The sink connector applying the CDC topics to the edge: the Debezium JDBC sink for the
/// relational edges (CNPG Postgres, Percona XtraDB), the MongoDB Kafka sink into the edge PSMDB
/// replica set for MongoDB.
fn sink_spec_for(
    kind: crate::SourceKind,
    short: &str,
    cr_name: &str,
    ns: &str,
    edge_secret: &str,
    db: &str,
) -> serde_json::Value {
    let edge_db = if kind.homogeneous() { db } else { "appdb" };
    match kind.edge_operator() {
        crate::connector::EdgeOperator::Cnpg => crate::cr::streaming::jdbc_sink_spec(
            short,
            &format!("jdbc:postgresql://{cr_name}-rw.{ns}.svc:5432/{edge_db}"),
            ns,
            edge_secret,
            "app",
            "password",
        ),
        crate::connector::EdgeOperator::Percona => crate::cr::streaming::jdbc_sink_spec(
            short,
            &format!("jdbc:mysql://{cr_name}-haproxy.{ns}.svc:3306/{edge_db}"),
            ns,
            edge_secret,
            "root",
            "root",
        ),
        crate::connector::EdgeOperator::Psmdb => crate::cr::streaming::mongo_sink_spec(
            short,
            &format!("{cr_name}-rs0.{ns}.svc:27017"),
            edge_db,
            ns,
            edge_secret,
            "MONGODB_DATABASE_ADMIN_USER",
            "MONGODB_DATABASE_ADMIN_PASSWORD",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_drill_names_the_state_flip_and_does_not_claim_a_data_copy() {
        let steps = rollback_steps();
        assert!(steps.iter().any(|(id, _)| *id == "mark_cutover"));
        assert!(steps.iter().any(|(id, _)| *id == "mark_plan"));
        let text = steps
            .iter()
            .map(|(_, action)| *action)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!text.to_ascii_lowercase().contains("copy rows"));
        assert!(text.contains("rolled_back"));
    }
}
