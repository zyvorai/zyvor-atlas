// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! REST surface. Slice 1 read-only inventory/discovery (PDF §10.2) + slice 2 async write path
//! (volume create/expand/delete, snapshots) — write ops return `202 Accepted` with a job id.

// pub(crate): `ai::compute_advisor`/`ai::AdvisorMode` are reused by the `ops_advisor` MCP tool
// (crates/atlas-gateway/src/mcp.rs), gated behind the `mcp` feature.
pub(crate) mod ai;
mod backends;
mod ceph_devices;
pub(crate) mod ceph_nvme;
pub(crate) mod ceph_nvmeof;
pub(crate) mod ceph_objectstore;
pub(crate) mod ceph_sizing;
pub(crate) mod databridge;
mod day2;
mod image_import;
pub(crate) mod dr;
pub(crate) mod governance;
pub(crate) mod inventory;
mod meta;
mod object_store;
pub(crate) mod observability;
mod oidc;
mod protection;
pub mod ransomware;
pub mod reduction;
mod dedup;
mod rbd;
mod rook;
pub(crate) mod util;
pub(crate) mod volume_groups;
pub(crate) mod volumes;
mod macos;
mod zfs;

use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{delete, get, post, put},
    Router,
};

use crate::auth::auth_middleware;
use crate::state::AppState;

use ai::*;
use backends::*;
use databridge::*;
use day2::*;
use dr::*;
use governance::*;
use inventory::*;
use meta::*;
use object_store::*;
use observability::*;
use oidc::*;
use protection::*;
use rbd::*;
use rook::*;
use volume_groups::*;
use volumes::*;
use macos::*;
use zfs::*;

pub fn router(state: AppState) -> Router {
    // Password login is intentionally outside the bearer middleware (this is how you get a token).
    // The OIDC routes below are the same idea — a browser-navigated login round-trip has no
    // bearer token to present yet by definition.
    let public_api = Router::new()
        .route("/auth/login", post(login))
        .route("/auth/oidc/status", get(oidc_status))
        .route("/auth/oidc/login", get(oidc_login))
        .route("/auth/oidc/callback", get(oidc_callback))
        .with_state(state.clone());

    let api = Router::new()
        .route("/ai/advisor", post(ai_advisor))
        .route("/ai/anomalies", get(ai_anomalies))
        .route(
            "/ai/ransomware/assess",
            post(ransomware::ai_ransomware_assess),
        )
        .route("/ai/incidents", get(ai_incidents))
        .route("/ai/what-if", post(ai_what_if))
        .route("/backends", get(list_backends).post(create_backend))
        .route("/backends/summary", get(backends_summary))
        .route("/backends/{id}", delete(delete_backend))
        .route("/backends/{id}/discover", post(discover_backend))
        .route("/backends/{id}/cordon", post(cordon_backend))
        .route("/backends/{id}/uncordon", post(uncordon_backend))
        .route("/maintenance", get(get_maintenance).post(set_maintenance))
        .route("/maintenance/orphans", get(list_orphans))
        .route("/upgrade/preflight", get(upgrade_preflight))
        .route("/dr/peers", get(list_dr_peers).post(register_dr_peer))
        .route("/dr/peers/{id}", delete(delete_dr_peer))
        .route("/dr/mirrors", get(list_dr_mirrors))
        .route("/dr/status", get(dr_status))
        .route("/dr/preflight", get(dr_preflight))
        .route("/dr/mirrors/{id}/status", get(mirror_status))
        .route("/dr/mirrors/{id}/promote", post(promote_mirror))
        .route("/dr/mirrors/{id}/demote", post(demote_mirror))
        .route("/dr/mirrors/{id}/resync", post(resync_mirror))
        .route("/dr/mirrors/{id}/rpo", post(set_mirror_rpo))
        .route("/dr/failover", post(dr_failover))
        .route(
            "/dr/pools/{pool}/mirroring",
            get(get_pool_mirroring).put(set_pool_mirroring),
        )
        .route(
            "/volume-groups",
            get(list_volume_groups).post(create_volume_group),
        )
        .route(
            "/volume-groups/{id}",
            get(get_volume_group).delete(delete_volume_group),
        )
        .route(
            "/volume-groups/{id}/snapshots",
            get(list_group_snapshots).post(create_group_snapshot),
        )
        .route(
            "/volume-groups/{id}/snapshots/{snap}",
            delete(delete_group_snapshot),
        )
        .route(
            "/volume-groups/{id}/snapshots/{snap}/rollback",
            post(rollback_group_snapshot),
        )
        .route("/protection-status", get(list_protection_status))
        .route(
            "/volumes/{id}/mirror",
            post(enable_mirror).delete(disable_mirror),
        )
        .route("/clusters", get(list_clusters))
        .route("/clusters/{id}/health", get(cluster_health))
        .route("/clusters/{id}/capabilities", get(cluster_capabilities))
        .route("/nodes", get(list_nodes))
        .route("/osds", get(list_osds))
        .route("/osds/ok-to-stop", get(osds_ok_to_stop))
        .route("/osds/{osd_id}/out", post(osd_out))
        .route("/osds/{osd_id}/in", post(osd_in))
        .route("/osds/{osd_id}/reweight", post(osd_reweight))
        .route("/pools", get(list_pools))
        .route("/ceph/status", get(get_ceph_status))
        .route("/ceph/osd-tree", get(get_ceph_osd_tree))
        .route("/ceph/osd-df", get(get_ceph_osd_df))
        .route("/ceph/devices", get(ceph_devices::get_ceph_devices))
        .route("/ceph/nvme", get(ceph_nvme::get_ceph_nvme))
        .route("/ceph/nvmeof", get(ceph_nvmeof::get_ceph_nvmeof))
        .route("/ceph/osd-objectstore", get(ceph_objectstore::get_ceph_osd_objectstore))
        .route("/ceph/sizing", get(ceph_sizing::get_ceph_sizing))
        .route("/ceph/df", get(get_ceph_df))
        .route("/ceph/health-rollup", get(get_ceph_health_rollup))
        .route("/ceph/rook-status", get(get_rook_status))
        .route("/ceph/pools", get(list_ceph_pools).post(create_ceph_pool))
        .route("/ceph/pools/{name}", delete(delete_ceph_pool))
        .route(
            "/ceph/filesystems",
            get(list_ceph_filesystems).post(create_ceph_filesystem),
        )
        .route("/ceph/filesystems/{name}", delete(delete_ceph_filesystem))
        .route(
            "/ceph/object-stores",
            get(list_ceph_object_stores).post(create_ceph_object_store),
        )
        .route(
            "/ceph/object-stores/{name}",
            delete(delete_ceph_object_store),
        )
        .route("/ceph/devices", post(add_ceph_device))
        .route(
            "/ceph/nvmeof/namespaces",
            post(ceph_nvmeof::create_nvmeof_namespace),
        )
        .route(
            "/ceph/nodes/{node_name}/devices",
            get(list_ceph_node_devices),
        )
        .route("/macos/devices", get(list_macos_devices))
        .route("/macos/disks/erase", post(erase_macos_disk))
        .route("/macos/disks/{device}/eject", post(eject_macos_disk))
        .route("/macos/containers/{container}/volumes", post(add_macos_volume))
        .route("/macos/containers/{container}/resize", post(resize_macos_container))
        .route("/macos/snapshots", get(list_macos_snapshots).post(create_macos_snapshot))
        .route("/macos/snapshots/{date}/delete", post(delete_macos_snapshot))
        .route("/macos/volumes/{device}/delete", post(delete_macos_volume))
        .route("/zfs/devices", get(list_zfs_devices))
        .route("/zfs/pools/from-device", post(create_zfs_pool_from_device))
        .route("/zfs/pools/{name}/destroy", post(destroy_zfs_pool))
        .route("/storage-classes", get(list_storage_classes))
        .route("/kubernetes/pvcs", get(list_pvcs))
        .route("/kubernetes/pvs", get(list_pvs))
        .route("/volumes", get(list_volumes).post(create_volume))
        .route("/volumes.csv", get(volumes_csv))
        .route("/volumes/{id}", get(get_volume).delete(delete_volume))
        .route("/volumes/{id}/expand", post(expand_volume))
        .route(
            "/volumes/{id}/data",
            get(read_volume_data).merge(
                put(write_volume_data).layer(DefaultBodyLimit::max(volumes::MAX_VOLUME_IO_BYTES)),
            ),
        )
        .route("/volumes/{id}/import", post(image_import::import_volume_image))
        .route("/volumes/{id}/protection", get(get_volume_protection))
        .route("/rbd-images", get(list_rbd_images).post(create_rbd_image))
        .route(
            "/rbd-images/{pool}/{image}",
            axum::routing::delete(delete_rbd_image),
        )
        .route("/rbd-images/{pool}/{image}/clone", post(clone_rbd_image))
        .route("/rbd-images/{pool}/{image}/resize", post(resize_rbd_image))
        .route(
            "/rbd-images/{pool}/{image}/migrate",
            post(migrate_rbd_image),
        )
        .route("/rbd-images/{pool}/{image}/qos", post(qos_rbd_image))
        .route(
            "/rbd-images/{pool}/{image}/flatten",
            post(flatten_rbd_image),
        )
        .route(
            "/rbd-images/{pool}/{image}/snapshots",
            get(list_rbd_snaps).post(create_rbd_snap),
        )
        .route(
            "/rbd-images/{pool}/{image}/snapshots/{snap}",
            axum::routing::delete(delete_rbd_snap),
        )
        .route(
            "/rbd-images/{pool}/{image}/rollback",
            post(rollback_rbd_image),
        )
        .route("/rbd-usage/refresh", post(refresh_rbd_usage))
        .route("/buckets/{id}/stats", get(bucket_stats))
        .route(
            "/buckets/{id}/objects",
            get(bucket_objects).delete(bucket_object_delete),
        )
        .route(
            "/buckets/{id}/objects/upload-url",
            post(bucket_object_upload_url),
        )
        .route(
            "/buckets/{id}/objects/download-url",
            get(bucket_object_download_url),
        )
        .route("/buckets/{id}/objects/prune", post(bucket_objects_prune))
        .route(
            "/buckets/{id}/subresource",
            get(bucket_subresource_get)
                .put(bucket_subresource_put)
                .delete(bucket_subresource_delete),
        )
        .route("/volumes/{id}/bindings", get(list_volume_bindings))
        .route(
            "/volumes/{id}/labels",
            get(get_volume_labels).put(put_volume_labels),
        )
        .route("/tenants", get(list_tenants))
        .route("/volumes/{id}/snapshots", post(create_snapshot))
        .route("/volumes/{id}/ransomware", get(ransomware::volume_ransomware))
        .route(
            "/volumes/{id}/ransomware/protect",
            post(ransomware::protect_volume),
        )
        .route("/native/reduction", get(reduction::native_reduction))
        .route("/pools/{id}/dedup-estimate", get(dedup::pool_dedup_estimate).post(dedup::start_pool_dedup_estimate))
        .route("/snapshots", get(list_snapshots))
        .route("/snapshots/{id}", axum::routing::delete(delete_snapshot))
        .route("/snapshots/{id}/lock", post(lock_snapshot))
        .route("/snapshots/{id}/clone", post(clone_snapshot))
        .route("/snapshots/{id}/restore", post(restore_snapshot))
        .route("/volumes/{id}/schedule", post(create_schedule))
        .route("/schedules", get(list_schedules))
        .route("/schedules/{id}", axum::routing::delete(delete_schedule))
        .route("/buckets", get(list_buckets).post(create_bucket))
        .route("/buckets/{id}", get(get_bucket).delete(delete_bucket))
        .route("/backup-jobs", post(create_backup))
        .route("/restore-jobs", post(create_restore))
        .route("/backups", get(list_backups))
        .route("/backups/{id}", get(get_backup).delete(delete_backup))
        .route("/backups/{id}/download", get(download_backup))
        // ---- DataBridge (cloud-to-edge DB migration) ----
        .route(
            "/databridge/sources",
            get(db_list_sources).post(db_create_source),
        )
        .route(
            "/databridge/sources/{id}",
            get(db_get_source).delete(db_delete_source),
        )
        .route(
            "/databridge/sources/{id}/discover",
            post(db_discover_source),
        )
        .route("/databridge/plans", get(db_list_plans).post(db_create_plan))
        .route(
            "/databridge/plans/{id}",
            get(db_get_plan).delete(db_delete_plan),
        )
        .route("/databridge/plans/{id}/assess", post(db_assess_plan))
        .route("/databridge/plans/{id}/provision", post(db_provision_edge))
        .route("/databridge/plans/{id}/full-load", post(db_full_load))
        .route("/databridge/plans/{id}/cdc/start", post(db_cdc_start))
        .route("/databridge/plans/{id}/cdc/stop", post(db_cdc_stop))
        .route("/databridge/plans/{id}/cdc/restart", post(db_cdc_restart))
        .route("/databridge/plans/{id}/validate", post(db_validate))
        .route("/databridge/plans/{id}/cutover", post(db_cutover))
        .route("/databridge/plans/{id}/rollback", post(db_rollback))
        // ---- DataBridge (object leg): cloud object store -> Ceph RGW ----
        .route(
            "/databridge/object",
            get(db_object_list).post(db_object_create),
        )
        .route(
            "/databridge/object/{id}",
            get(db_object_get).delete(db_object_delete),
        )
        .route("/databridge/object/{id}/start", post(db_object_start))
        .route("/databridge/edge-clusters", get(db_list_edge_clusters))
        .route(
            "/databridge/edge-clusters/{id}",
            get(db_get_edge_cluster).delete(db_delete_edge_cluster),
        )
        .route("/databridge/cdc-streams", get(db_list_cdc_streams))
        .route("/databridge/cdc-streams/{id}", get(db_get_cdc_stream))
        .route("/databridge/validations", get(db_list_validations))
        .route("/databridge/cutovers", get(db_list_cutovers))
        .route("/policies", get(list_policies))
        .route("/auth/tokens", post(issue_token))
        .route("/auth/tokens/revoked", get(list_revoked_tokens))
        .route("/auth/tokens/{jti}/revoke", post(revoke_token))
        .route("/auth/users", get(list_users).post(create_user))
        .route(
            "/auth/users/{username}",
            axum::routing::put(update_user).delete(delete_user),
        )
        .route(
            "/tenants/{id}/quota",
            get(get_tenant_quota).put(put_tenant_quota),
        )
        .route("/tenants/{id}/policies", get(list_tenant_policies))
        .route(
            "/tenants/{id}/policies/{intent}",
            axum::routing::put(put_tenant_policy).delete(delete_tenant_policy),
        )
        .route("/metrics/summary", get(metrics_summary))
        .route("/metrics/ceph", get(metrics_ceph))
        .route("/metrics/history", get(metrics_history))
        .route("/metrics/forecast", get(metrics_forecast))
        .route("/alerts", get(list_alerts))
        .route("/alerts/evaluate", post(evaluate_alerts))
        .route("/alerts/{id}/ack", post(ack_alert))
        .route("/alerts/{id}/silence", post(silence_alert))
        .route("/alerts/{id}/resolve", post(resolve_alert))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
        .route("/jobs/{id}/watch", get(watch_job_sse))
        .route("/jobs/{id}/cancel", post(cancel_job))
        .route("/audit", get(list_audit))
        .route("/audit.csv", get(export_audit_csv))
        .route("/events", get(list_events))
        .route("/chargeback", get(chargeback))
        .route("/policy-drift", get(policy_drift));

    // Nested (not a separate listener like the gRPC edge): MCP-over-streamable-HTTP is plain
    // HTTP, so it rides the same axum Router, TLS listener, and `auth_middleware` bearer-JWT gate
    // as the rest of the API — see crates/atlas-gateway/src/mcp.rs.
    #[cfg(feature = "mcp")]
    let api = api.nest_service("/mcp", crate::mcp::router(state.clone()));

    let api = api
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state.clone());

    // `/metrics` exposes real inventory counts, backend names, and capacity — gate it behind the
    // same bearer-token check as the rest of the API so it can't be scraped anonymously off a
    // publicly reachable NodePort. Prometheus authenticates via `authorization.credentials_file`
    // (see deploy/observability/prometheus.yaml).
    let metrics = Router::new()
        .route("/metrics", get(prometheus_metrics))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state.clone());

    let root = Router::new()
        .route("/health", get(health))
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/version", get(version))
        .merge(metrics)
        .nest("/api/atlas/v1", public_api.merge(api));
    // Public and static (no tools, no inventory), so agent hosts can find the endpoint and auth scheme.
    #[cfg(feature = "mcp")]
    let root = root.route("/.well-known/mcp", get(crate::mcp::server_card));
    root
        // Any other path serves the embedded Storage Center SPA (client-side routing).
        .fallback(get(spa_handler))
        .with_state(state)
}
