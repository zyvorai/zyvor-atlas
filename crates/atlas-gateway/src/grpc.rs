// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! gRPC edge (PDF §5.3): a typed contract for products, served alongside REST over the same
//! `AppState`. Read RPCs plus the async volume-create path (returns a job id).

use std::pin::Pin;

use tokio_stream::Stream;
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::{Request, Response, Status};

use atlas_api_types::VolumeKind;

use crate::proto::atlas_storage_server::{AtlasStorage, AtlasStorageServer};
use crate::proto::{
    Alert, Bucket, Cluster, CreateSnapshotRequest, CreateVolumeReply, CreateVolumeRequest,
    DeleteVolumeRequest, Empty, ExpandVolumeRequest, GetJobRequest, GetVolumeRequest, HealthReply,
    HealthRequest, Job, JobReply, ListAlertsReply, ListAlertsRequest, ListBucketsReply,
    ListClustersReply, ListJobsReply, ListJobsRequest, ListPoolsReply, ListSnapshotsReply,
    ListSnapshotsRequest, ListTenantsReply, ListVolumesByOwnerRequest, ListVolumesReply,
    MetricsSummary, Pool, Snapshot, Tenant, Volume,
};
use crate::state::AppState;

/// The authenticated actor (id + role), injected into request extensions by the auth interceptor.
#[derive(Clone)]
pub struct GrpcActor {
    pub id: String,
    pub role: String,
}

/// Build the tonic service with a JWT auth interceptor. When `auth_required` is false (dev), calls
/// pass through as `anonymous`; when true, a valid HS256 Bearer token in the `authorization`
/// metadata is required (mirrors the REST middleware).
// tonic's Interceptor must return `Result<_, Status>`; Status is large but this is the required
// signature, so silence the size lint.
#[allow(clippy::result_large_err)]
pub fn service(
    state: AppState,
) -> InterceptedService<AtlasStorageServer<GrpcService>, impl Interceptor + Clone> {
    let secret = state.config.jwt_secret.clone();
    let secret_previous = state.config.jwt_secret_previous.clone();
    let required = state.config.auth_required;
    let bootstrap = state.config.bootstrap_admin_token.clone();
    let rate = state.rate.clone();
    AtlasStorageServer::with_interceptor(
        GrpcService { state },
        move |mut req: Request<()>| -> Result<Request<()>, Status> {
            let actor = if !required {
                GrpcActor {
                    id: "anonymous".into(),
                    role: "viewer".into(),
                }
            } else {
                let token = req
                    .metadata()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.strip_prefix("Bearer "));
                let Some(token) = token else {
                    return Err(Status::unauthenticated("missing bearer token"));
                };
                if bootstrap.as_deref().is_some_and(|boot| boot == token) {
                    GrpcActor {
                        id: "bootstrap".into(),
                        role: "admin".into(),
                    }
                } else {
                    match crate::auth::decode_token(token, &secret, secret_previous.as_deref()) {
                        // A scoped agent token is MCP-only (see `Claims::tools`).
                        Ok(data) if !data.claims.tools.is_empty() => {
                            return Err(Status::permission_denied(
                                "this token is scoped to MCP tools and is not valid on gRPC",
                            ))
                        }
                        Ok(data) => GrpcActor {
                            id: data.claims.sub,
                            role: data.claims.role,
                        },
                        Err(e) => {
                            return Err(Status::unauthenticated(format!("invalid token: {e}")))
                        }
                    }
                }
            };
            // Per-actor rate limit (mirrors the REST auth_middleware; disabled unless
            // ATLAS_RATE_LIMIT_RPM > 0), so the gRPC edge can't bypass the REST governance limit.
            if !rate.allow(&actor.id) {
                return Err(Status::resource_exhausted("rate limit exceeded"));
            }
            req.extensions_mut().insert(actor);
            Ok(req)
        },
    )
}

pub struct GrpcService {
    state: AppState,
}

fn internal(e: impl std::fmt::Display) -> Status {
    Status::internal(e.to_string())
}

impl GrpcService {
    /// Resolve the request actor and enforce a minimum role (only when auth is enabled). Returns the
    /// actor id to attribute the job to, or `permission_denied`. Mirrors the REST `require_role`.
    #[allow(clippy::result_large_err)]
    fn require_role<T>(&self, req: &Request<T>, min_role: u8) -> Result<String, Status> {
        let ga = req.extensions().get::<GrpcActor>().cloned();
        let actor = ga
            .as_ref()
            .map(|a| a.id.clone())
            .unwrap_or_else(|| "grpc".into());
        if self.state.config.auth_required {
            let role = ga.as_ref().map(|a| a.role.as_str()).unwrap_or("viewer");
            if crate::auth::role_level(role) < min_role {
                return Err(Status::permission_denied("insufficient role"));
            }
        }
        Ok(actor)
    }
}

fn kind_str(k: VolumeKind) -> String {
    match k {
        VolumeKind::Block => "block",
        VolumeKind::Filesystem => "filesystem",
        VolumeKind::Object => "object",
    }
    .into()
}

fn health_str(h: atlas_api_types::Health) -> String {
    match h {
        atlas_api_types::Health::Ok => "ok",
        atlas_api_types::Health::Warn => "warn",
        atlas_api_types::Health::Critical => "critical",
        atlas_api_types::Health::Unknown => "unknown",
    }
    .into()
}

#[tonic::async_trait]
impl AtlasStorage for GrpcService {
    async fn health(&self, _req: Request<HealthRequest>) -> Result<Response<HealthReply>, Status> {
        Ok(Response::new(HealthReply {
            status: "ok".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        }))
    }

    async fn list_clusters(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<ListClustersReply>, Status> {
        let clusters = atlas_inventory::list_clusters(&self.state.pool)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|c| Cluster {
                id: c.id,
                name: c.name,
                health: health_str(c.health),
                raw_capacity_bytes: c.raw_capacity_bytes.unwrap_or(0),
                used_capacity_bytes: c.used_capacity_bytes.unwrap_or(0),
                available_capacity_bytes: c.available_capacity_bytes.unwrap_or(0),
            })
            .collect();
        Ok(Response::new(ListClustersReply { clusters }))
    }

    async fn list_pools(&self, _req: Request<Empty>) -> Result<Response<ListPoolsReply>, Status> {
        let pools = atlas_inventory::list_pools(&self.state.pool)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|p| Pool {
                id: p.id,
                name: p.name,
                kind: p.kind,
                used_bytes: p.used_bytes.unwrap_or(0),
                max_bytes: p.max_bytes.unwrap_or(0),
                health: health_str(p.health),
            })
            .collect();
        Ok(Response::new(ListPoolsReply { pools }))
    }

    async fn list_volumes(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<ListVolumesReply>, Status> {
        let volumes = atlas_inventory::list_volumes(&self.state.pool)
            .await
            .map_err(internal)?
            .into_iter()
            .map(volume_to_proto)
            .collect();
        Ok(Response::new(ListVolumesReply { volumes }))
    }

    async fn get_volume(&self, req: Request<GetVolumeRequest>) -> Result<Response<Volume>, Status> {
        let id = req.into_inner().id;
        let v = atlas_inventory::get_volume(&self.state.pool, &id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("volume {id}")))?;
        Ok(Response::new(volume_to_proto(v)))
    }

    async fn create_volume(
        &self,
        req: Request<CreateVolumeRequest>,
    ) -> Result<Response<CreateVolumeReply>, Status> {
        let ga = req.extensions().get::<GrpcActor>().cloned();
        let actor = ga
            .as_ref()
            .map(|a| a.id.clone())
            .unwrap_or_else(|| "grpc".into());
        // RBAC: creating a volume requires operator (enforced only when auth is on).
        if self.state.config.auth_required {
            let role = ga.as_ref().map(|a| a.role.as_str()).unwrap_or("viewer");
            if crate::auth::role_level(role) < crate::auth::ROLE_OPERATOR {
                return Err(Status::permission_denied("requires operator role"));
            }
        }
        let r = req.into_inner();
        if r.name.trim().is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        if r.size_bytes <= 0 {
            return Err(Status::invalid_argument("size_bytes must be > 0"));
        }
        let policy = (!r.policy.is_empty()).then_some(r.policy.as_str());
        let placement = atlas_policy::resolve(policy, VolumeKind::Block, None)
            .map_err(Status::invalid_argument)?;
        let namespace = if r.namespace.is_empty() {
            "default".to_string()
        } else {
            r.namespace
        };
        let tenant_id = if r.tenant_id.is_empty() {
            "global".to_string()
        } else {
            r.tenant_id
        };
        // Tenant quota admission (PDF §14): reject a create that would exceed the tenant's limits.
        match atlas_inventory::tenants::check_admission(&self.state.pool, &tenant_id, r.size_bytes)
            .await
            .map_err(internal)?
        {
            atlas_inventory::tenants::QuotaCheck::Ok => {}
            atlas_inventory::tenants::QuotaCheck::Bytes { limit, would_be } => {
                return Err(Status::resource_exhausted(format!(
                    "tenant {tenant_id} byte quota exceeded: {would_be} > {limit}"
                )));
            }
            atlas_inventory::tenants::QuotaCheck::Count { limit, current } => {
                return Err(Status::resource_exhausted(format!(
                    "tenant {tenant_id} volume-count quota exceeded: {current} already at limit {limit}"
                )));
            }
        }
        let volume_id = atlas_common::ids::volume_id();
        let job_id = atlas_common::ids::job_id();
        let idem = atlas_common::ids::stable_id(
            "idem",
            &format!("{}|{}|{}|volume.create", tenant_id, r.name, r.size_bytes),
        );
        let owner =
            r.owner
                .as_ref()
                .filter(|o| !o.product.is_empty())
                .map(|o| atlas_jobs::OwnerRef {
                    product: o.product.clone(),
                    resource_type: o.resource_type.clone(),
                    resource_id: o.resource_id.clone(),
                    role: if o.role.is_empty() {
                        "owner".into()
                    } else {
                        o.role.clone()
                    },
                });
        let spec = atlas_jobs::JobSpec::VolumeCreate {
            volume_id: volume_id.clone(),
            backend_id: CEPH_BACKEND_ID.into(),
            name: r.name,
            namespace,
            storage_class: placement.storage_class,
            access_mode: placement.access_mode,
            volume_mode: placement.volume_mode,
            size_bytes: r.size_bytes,
            kind: "block".into(),
            policy: Some(placement.intent),
            owner,
        };
        let job = self
            .state
            .jobs
            .enqueue(&job_id, &tenant_id, &actor, spec, Some(&idem))
            .await
            .map_err(internal)?;
        Ok(Response::new(CreateVolumeReply {
            job_id: job.id,
            volume_id,
            state: job.state,
        }))
    }

    async fn delete_volume(
        &self,
        req: Request<DeleteVolumeRequest>,
    ) -> Result<Response<JobReply>, Status> {
        let actor = self.require_role(&req, crate::auth::ROLE_ADMIN)?;
        let body = req.into_inner();
        let id = body.id;
        let vol = atlas_inventory::get_volume(&self.state.pool, &id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("volume {id}")))?;
        let sc = vol
            .storage_class_name
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        let needs_confirm =
            sc.contains("prod") || sc.contains("production") || sc.contains("database");
        if needs_confirm && !body.confirm {
            return Err(Status::failed_precondition(
                "confirm=true is required to delete this volume (production / protected class)",
            ));
        }
        let namespace = vol
            .kubernetes_namespace
            .ok_or_else(|| Status::failed_precondition("volume has no kubernetes namespace"))?;
        let pvc_name = vol
            .pvc_name
            .ok_or_else(|| Status::failed_precondition("volume has no pvc"))?;
        let job_id = atlas_common::ids::job_id();
        let spec = atlas_jobs::JobSpec::VolumeDelete {
            volume_id: id.clone(),
            namespace,
            pvc_name,
        };
        let job = self
            .state
            .jobs
            .enqueue(&job_id, "global", &actor, spec, None)
            .await
            .map_err(internal)?;
        Ok(Response::new(JobReply {
            job_id: job.id,
            state: job.state,
        }))
    }

    async fn expand_volume(
        &self,
        req: Request<ExpandVolumeRequest>,
    ) -> Result<Response<JobReply>, Status> {
        let actor = self.require_role(&req, crate::auth::ROLE_OPERATOR)?;
        let r = req.into_inner();
        let vol = atlas_inventory::get_volume(&self.state.pool, &r.id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("volume {}", r.id)))?;
        if r.new_size_bytes <= vol.size_bytes {
            return Err(Status::invalid_argument(
                "new_size_bytes must be larger than the current size",
            ));
        }
        let namespace = vol
            .kubernetes_namespace
            .ok_or_else(|| Status::failed_precondition("volume has no kubernetes namespace"))?;
        let pvc_name = vol
            .pvc_name
            .ok_or_else(|| Status::failed_precondition("volume has no pvc"))?;
        let job_id = atlas_common::ids::job_id();
        let spec = atlas_jobs::JobSpec::VolumeExpand {
            volume_id: r.id.clone(),
            namespace,
            pvc_name,
            new_size_bytes: r.new_size_bytes,
        };
        let job = self
            .state
            .jobs
            .enqueue(&job_id, "global", &actor, spec, None)
            .await
            .map_err(internal)?;
        Ok(Response::new(JobReply {
            job_id: job.id,
            state: job.state,
        }))
    }

    async fn list_snapshots(
        &self,
        req: Request<ListSnapshotsRequest>,
    ) -> Result<Response<ListSnapshotsReply>, Status> {
        let vid = req.into_inner().volume_id;
        let filter = (!vid.trim().is_empty()).then_some(vid.as_str());
        let snapshots = atlas_inventory::snapshots::list_snapshots(&self.state.pool, filter)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|s| Snapshot {
                id: s.id,
                volume_id: s.volume_id,
                name: s.name,
                state: s.state,
                protected: s.protected,
                created_at: s.created_at.unwrap_or_default(),
            })
            .collect();
        Ok(Response::new(ListSnapshotsReply { snapshots }))
    }

    async fn create_snapshot(
        &self,
        req: Request<CreateSnapshotRequest>,
    ) -> Result<Response<JobReply>, Status> {
        let actor = self.require_role(&req, crate::auth::ROLE_OPERATOR)?;
        let r = req.into_inner();
        if r.volume_id.trim().is_empty() {
            return Err(Status::invalid_argument("volume_id is required"));
        }
        let vol = atlas_inventory::get_volume(&self.state.pool, &r.volume_id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("volume {}", r.volume_id)))?;
        let namespace = vol
            .kubernetes_namespace
            .ok_or_else(|| Status::failed_precondition("volume has no kubernetes namespace"))?;
        let pvc_name = vol
            .pvc_name
            .ok_or_else(|| Status::failed_precondition("volume has no pvc"))?;
        let snapshot_id = atlas_common::ids::snapshot_id();
        let name = if r.name.trim().is_empty() {
            format!("{}-{}", vol.name, &snapshot_id[5..])
        } else {
            r.name
        };
        let job_id = atlas_common::ids::job_id();
        let spec = atlas_jobs::JobSpec::SnapshotCreate {
            snapshot_id,
            volume_id: r.volume_id,
            name,
            namespace,
            pvc_name,
            snapshot_class: "zyvor-rbd-snapclass".into(),
        };
        let job = self
            .state
            .jobs
            .enqueue(&job_id, "global", &actor, spec, None)
            .await
            .map_err(internal)?;
        Ok(Response::new(JobReply {
            job_id: job.id,
            state: job.state,
        }))
    }

    async fn list_volumes_by_owner(
        &self,
        req: Request<ListVolumesByOwnerRequest>,
    ) -> Result<Response<ListVolumesReply>, Status> {
        let r = req.into_inner();
        if r.product.trim().is_empty() {
            return Err(Status::invalid_argument("product is required"));
        }
        let resource_id = (!r.resource_id.trim().is_empty()).then_some(r.resource_id.as_str());
        let volumes =
            atlas_inventory::list_volumes_by_owner(&self.state.pool, &r.product, resource_id)
                .await
                .map_err(internal)?
                .into_iter()
                .map(volume_to_proto)
                .collect();
        Ok(Response::new(ListVolumesReply { volumes }))
    }

    async fn get_job(&self, req: Request<GetJobRequest>) -> Result<Response<Job>, Status> {
        let id = req.into_inner().id;
        let j = atlas_inventory::jobs::get_job(&self.state.pool, &id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("job {id}")))?;
        Ok(Response::new(job_to_proto(j)))
    }

    async fn list_jobs(
        &self,
        req: Request<ListJobsRequest>,
    ) -> Result<Response<ListJobsReply>, Status> {
        let r = req.into_inner();
        let limit = if r.limit <= 0 { 50 } else { i64::from(r.limit) };
        let state = (!r.state.trim().is_empty()).then_some(r.state.as_str());
        let jobs = atlas_inventory::jobs::list_jobs_filtered(&self.state.pool, state, limit)
            .await
            .map_err(internal)?
            .into_iter()
            .map(job_to_proto)
            .collect();
        Ok(Response::new(ListJobsReply { jobs }))
    }

    type WatchJobStream = Pin<Box<dyn Stream<Item = Result<Job, Status>> + Send>>;

    /// Stream a job on each state change until it reaches a terminal state (or a ~2 min cap).
    async fn watch_job(
        &self,
        req: Request<GetJobRequest>,
    ) -> Result<Response<Self::WatchJobStream>, Status> {
        let id = req.into_inner().id;
        let pool = self.state.pool.clone();
        let stream = async_stream::try_stream! {
            let mut last = String::new();
            for _ in 0..240 {
                match atlas_inventory::jobs::get_job(&pool, &id).await.map_err(internal)? {
                    Some(j) => {
                        let terminal = j.state == "succeeded" || j.state == "failed";
                        if j.state != last {
                            last = j.state.clone();
                            yield job_to_proto(j);
                        }
                        if terminal {
                            break;
                        }
                    }
                    None => Err(Status::not_found(format!("job {id}")))?,
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        };
        Ok(Response::new(Box::pin(stream) as Self::WatchJobStream))
    }

    async fn list_alerts(
        &self,
        req: Request<ListAlertsRequest>,
    ) -> Result<Response<ListAlertsReply>, Status> {
        let state = req.into_inner().state;
        let filter = (!state.is_empty()).then_some(state.as_str());
        let alerts = atlas_inventory::alerts::list(&self.state.pool, filter)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|a| Alert {
                id: a.id,
                severity: a.severity,
                title: a.title,
                description: a.description,
                state: a.state,
                resource_id: a.resource_id,
            })
            .collect();
        Ok(Response::new(ListAlertsReply { alerts }))
    }

    async fn get_metrics_summary(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<MetricsSummary>, Status> {
        let v = atlas_inventory::metrics_summary(&self.state.pool)
            .await
            .map_err(internal)?;
        let io = v.get("client_io").cloned().unwrap_or(serde_json::json!({}));
        let rec = v.get("recovery").cloned().unwrap_or(serde_json::json!({}));
        Ok(Response::new(MetricsSummary {
            raw_capacity_bytes: v["raw_capacity_bytes"].as_i64().unwrap_or(0),
            used_capacity_bytes: v["used_capacity_bytes"].as_i64().unwrap_or(0),
            available_capacity_bytes: v["available_capacity_bytes"].as_i64().unwrap_or(0),
            used_capacity_percent: v["used_capacity_percent"].as_f64().unwrap_or(0.0),
            clusters: v["clusters"].as_i64().unwrap_or(0),
            pools: v["pools"].as_i64().unwrap_or(0),
            volumes: v["volumes"].as_i64().unwrap_or(0),
            snapshots: v["snapshots"].as_i64().unwrap_or(0),
            buckets: v["buckets"].as_i64().unwrap_or(0),
            backups: v["backups"].as_i64().unwrap_or(0),
            read_ops_total: io["read_ops_total"].as_f64().unwrap_or(0.0),
            write_ops_total: io["write_ops_total"].as_f64().unwrap_or(0.0),
            pg_recovering: rec["pg_recovering"].as_i64().unwrap_or(0),
            pg_backfilling: rec["pg_backfilling"].as_i64().unwrap_or(0),
            objects_degraded: rec["objects_degraded"].as_i64().unwrap_or(0),
        }))
    }

    async fn list_buckets(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<ListBucketsReply>, Status> {
        let buckets = atlas_inventory::buckets::list_buckets(&self.state.pool)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|b| Bucket {
                id: b.id,
                name: b.name,
                bucket_name: b.bucket_name.unwrap_or_default(),
                state: b.state,
                tenant_id: b.tenant_id,
                namespace: b.namespace.unwrap_or_default(),
                endpoint: b.endpoint.unwrap_or_default(),
            })
            .collect();
        Ok(Response::new(ListBucketsReply { buckets }))
    }

    async fn list_tenants(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<ListTenantsReply>, Status> {
        // Inventory overview keys tenants by `tenant_id` only (no separate display name).
        let tenants = atlas_inventory::tenants::list_overview(&self.state.pool)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|t| Tenant {
                id: t.tenant_id.clone(),
                name: t.tenant_id,
            })
            .collect();
        Ok(Response::new(ListTenantsReply { tenants }))
    }
}

/// The single Ceph backend id (mirrors startup::CEPH_BACKEND_ID).
pub const CEPH_BACKEND_ID: &str = "bkd_ceph_lab";

fn job_to_proto(j: atlas_api_types::JobRecord) -> Job {
    Job {
        id: j.id,
        job_type: j.job_type,
        state: j.state,
        progress_percent: j.progress_percent,
        error: j.error.unwrap_or_default(),
    }
}

fn volume_to_proto(v: atlas_api_types::StorageVolume) -> Volume {
    Volume {
        id: v.id,
        name: v.name,
        kind: kind_str(v.kind),
        size_bytes: v.size_bytes,
        state: v.state,
        pvc_name: v.pvc_name.unwrap_or_default(),
        storage_class: v.storage_class_name.unwrap_or_default(),
        namespace: v.kubernetes_namespace.unwrap_or_default(),
    }
}
