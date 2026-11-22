// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The CSI controller service: a volume is an atlas-native filesystem, a snapshot a filesystem
//! snapshot. Ids are derived from the CSI names, so a retried create lands on the object the
//! first attempt made (the cluster treats a create with an existing id and the same name as done).
//! A volume's capacity is its filesystem's byte quota (unless the StorageClass sets
//! `enforceCapacity: "false"`), so expanding a PVC raises the quota and needs no node step.

use std::sync::Arc;

use atlas_native_fuse::client::{encode, Body, Client, Error as ClientError, Retry};
use reqwest::Method;
use serde_json::{json, Value};
use tonic::{Request, Response, Status};

use crate::proto::{
    controller_server::Controller, controller_service_capability,
    validate_volume_capabilities_response, volume_capability, volume_content_source, CapacityRange,
    ControllerExpandVolumeRequest, ControllerExpandVolumeResponse,
    ControllerGetCapabilitiesRequest, ControllerGetCapabilitiesResponse,
    ControllerServiceCapability, CreateSnapshotRequest, CreateSnapshotResponse,
    CreateVolumeRequest, CreateVolumeResponse, DeleteSnapshotRequest, DeleteSnapshotResponse,
    DeleteVolumeRequest, DeleteVolumeResponse, Snapshot, Timestamp,
    ValidateVolumeCapabilitiesRequest, ValidateVolumeCapabilitiesResponse, Volume,
    VolumeCapability,
};

/// StorageClass parameter: the filesystem's extent grid in bytes.
pub const PARAM_EXTENT_BYTES: &str = "extentBytes";
/// StorageClass parameter: `"false"` leaves the filesystem without a byte quota (thin, as
/// before quotas existed); the default enforces the PVC's size.
pub const PARAM_ENFORCE_CAPACITY: &str = "enforceCapacity";

pub struct ControllerService {
    client: Arc<Client>,
}

impl ControllerService {
    pub fn new(client: Client) -> Self {
        Self {
            client: Arc::new(client),
        }
    }

    async fn call(
        &self,
        method: Method,
        path: String,
        body: Option<Value>,
        retry: Retry,
    ) -> Result<Vec<u8>, ClientError> {
        let client = self.client.clone();
        tokio::task::spawn_blocking(move || {
            client.request(method, &path, body.map_or(Body::Empty, Body::Json), retry)
        })
        .await
        .unwrap_or_else(|e| Err(ClientError::Transport(format!("request task: {e}"))))
    }

    async fn post(&self, path: String, body: Value) -> Result<(), ClientError> {
        self.call(Method::POST, path, Some(body), Retry::Idempotent)
            .await
            .map(drop)
    }

    /// Sets the byte limit of filesystem `id`'s quota, keeping any inode limit; `None` clears it.
    async fn set_max_bytes(&self, id: &str, max_bytes: Option<u64>) -> Result<(), ClientError> {
        let path = format!("/v1/fs/{}/quota", encode(id));
        let mut quota = self.quota(id).await?;
        if quota["max_bytes"].as_u64() == max_bytes {
            return Ok(());
        }
        match max_bytes {
            Some(n) => quota["max_bytes"] = n.into(),
            None => {
                quota.as_object_mut().map(|o| o.remove("max_bytes"));
            }
        }
        self.call(Method::PUT, path, Some(quota), Retry::Idempotent)
            .await
            .map(drop)
    }

    /// Filesystem `id`'s quota as the cluster returns it (`{}` without one).
    async fn quota(&self, id: &str) -> Result<Value, ClientError> {
        let body = self
            .call(
                Method::GET,
                format!("/v1/fs/{}/quota", encode(id)),
                None,
                Retry::Idempotent,
            )
            .await?;
        serde_json::from_slice(&body)
            .map_err(|e| ClientError::Transport(format!("quota response: {e}")))
    }

    /// Deletes `path`; a missing object counts as deleted.
    async fn remove(&self, path: String) -> Result<(), ClientError> {
        match self.call(Method::DELETE, path, None, Retry::Remove).await {
            Err(e) if e.is_not_found() => Ok(()),
            r => r.map(drop),
        }
    }
}

/// Whether `id` is a valid atlas-native client id (1-64 of `[A-Za-z0-9-]`).
pub fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The object id for CSI name `name`: the name itself when it is a short valid id (the
/// provisioner's `pvc-<uuid>` and `snapshot-<uuid>`), else a hash of it. Short enough to take
/// the `-src` suffix of a volume clone's transient snapshot.
pub fn object_id(name: &str) -> String {
    if name.len() <= 48 && valid_id(name) {
        return name.to_string();
    }
    let hex: String = atlas_native::checksum::sha256(name.as_bytes())
        .iter()
        .take(20)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("csi-{hex}")
}

fn status(e: ClientError) -> Status {
    match &e {
        ClientError::Api {
            status: code_num,
            code,
            message,
        } => match code.as_str() {
            "not_found" => Status::not_found(message.clone()),
            "exists" => Status::already_exists(message.clone()),
            "invalid" => Status::invalid_argument(message.clone()),
            "not_empty" | "locked" | "read_only" => Status::failed_precondition(message.clone()),
            "quota" => Status::resource_exhausted(message.clone()),
            _ if *code_num == 401 => Status::unauthenticated(e.to_string()),
            _ if *code_num == 403 => Status::permission_denied(e.to_string()),
            _ => Status::internal(e.to_string()),
        },
        ClientError::Transport(_) => Status::unavailable(e.to_string()),
        _ => Status::internal(e.to_string()),
    }
}

/// Every capability is a filesystem mount; block access is unsupported. All access modes work
/// (each publish is its own FUSE client on a shared, lease-coherent filesystem).
fn unsupported(caps: &[VolumeCapability]) -> Option<String> {
    caps.iter().find_map(|c| match c.access_type {
        Some(volume_capability::AccessType::Mount(_)) => None,
        Some(volume_capability::AccessType::Block(_)) => {
            Some("block volumes are not supported; use a filesystem volume".into())
        }
        None => Some("a volume capability needs an access type".into()),
    })
}

type Params = std::collections::HashMap<String, String>;

fn check_params(params: &Params) -> Result<(), Status> {
    for k in params.keys() {
        if k != PARAM_EXTENT_BYTES
            && k != PARAM_ENFORCE_CAPACITY
            && !k.starts_with("csi.storage.k8s.io/")
        {
            return Err(Status::invalid_argument(format!(
                "unknown parameter {k:?} (supported: {PARAM_EXTENT_BYTES}, {PARAM_ENFORCE_CAPACITY})"
            )));
        }
    }
    Ok(())
}

fn enforce_capacity(params: &Params) -> Result<bool, Status> {
    match params.get(PARAM_ENFORCE_CAPACITY).map(String::as_str) {
        None | Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(v) => Err(Status::invalid_argument(format!(
            "{PARAM_ENFORCE_CAPACITY} must be \"true\" or \"false\", got {v:?}"
        ))),
    }
}

/// The capacity a range asks for: `required_bytes`, else `limit_bytes`, else 0 (unspecified).
fn requested(range: Option<&CapacityRange>) -> Result<u64, Status> {
    let Some(r) = range else { return Ok(0) };
    if r.required_bytes < 0 || r.limit_bytes < 0 {
        return Err(Status::invalid_argument(
            "capacity_range must not be negative",
        ));
    }
    if r.limit_bytes > 0 && r.required_bytes > r.limit_bytes {
        return Err(Status::out_of_range(format!(
            "required_bytes {} exceeds limit_bytes {}",
            r.required_bytes, r.limit_bytes
        )));
    }
    Ok(if r.required_bytes > 0 {
        r.required_bytes
    } else {
        r.limit_bytes
    } as u64)
}

fn extent_bytes(params: &Params) -> Result<Option<u64>, Status> {
    params
        .get(PARAM_EXTENT_BYTES)
        .map(|v| match v.parse::<u64>() {
            Ok(n) if n > 0 => Ok(n),
            _ => Err(Status::invalid_argument(format!(
                "{PARAM_EXTENT_BYTES} must be a positive integer, got {v:?}"
            ))),
        })
        .transpose()
}

fn rpc(t: controller_service_capability::rpc::Type) -> ControllerServiceCapability {
    ControllerServiceCapability {
        r#type: Some(controller_service_capability::Type::Rpc(
            controller_service_capability::Rpc { r#type: t as i32 },
        )),
    }
}

#[tonic::async_trait]
impl Controller for ControllerService {
    async fn create_volume(
        &self,
        req: Request<CreateVolumeRequest>,
    ) -> Result<Response<CreateVolumeResponse>, Status> {
        let r = req.into_inner();
        if r.name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        if r.volume_capabilities.is_empty() {
            return Err(Status::invalid_argument("volume_capabilities are required"));
        }
        if let Some(why) = unsupported(&r.volume_capabilities) {
            return Err(Status::invalid_argument(why));
        }
        check_params(&r.parameters)?;
        let extent = extent_bytes(&r.parameters)?;
        let enforce = enforce_capacity(&r.parameters)?;
        let capacity = requested(r.capacity_range.as_ref())?;
        let id = object_id(&r.name);
        let source = r
            .volume_content_source
            .as_ref()
            .and_then(|s| s.r#type.as_ref());
        match source {
            None => {
                self.post(
                    "/v1/fs".into(),
                    json!({ "id": id, "name": r.name, "extent_bytes": extent }),
                )
                .await
                .map_err(status)?;
            }
            Some(volume_content_source::Type::Snapshot(s)) => {
                if !valid_id(&s.snapshot_id) {
                    return Err(Status::not_found(format!("snapshot {}", s.snapshot_id)));
                }
                self.post(
                    format!("/v1/fs-snapshots/{}/clone", encode(&s.snapshot_id)),
                    json!({ "id": id, "name": r.name }),
                )
                .await
                .map_err(status)?;
            }
            Some(volume_content_source::Type::Volume(v)) => {
                if !valid_id(&v.volume_id) {
                    return Err(Status::not_found(format!("volume {}", v.volume_id)));
                }
                // A volume clone is a clone of a transient snapshot, deleted once the clone
                // exists (the clone keeps its own references to the shared extents).
                let snap = format!("{id}-src");
                self.post(
                    format!("/v1/fs/{}/snapshots", encode(&v.volume_id)),
                    json!({ "id": snap, "name": snap }),
                )
                .await
                .map_err(status)?;
                self.post(
                    format!("/v1/fs-snapshots/{snap}/clone"),
                    json!({ "id": id, "name": r.name }),
                )
                .await
                .map_err(status)?;
                self.remove(format!("/v1/fs-snapshots/{snap}"))
                    .await
                    .map_err(status)?;
            }
        }
        // A clone inherits its source's quota; replace the byte limit with this volume's own.
        let max_bytes = (enforce && capacity > 0).then_some(capacity);
        self.set_max_bytes(&id, max_bytes).await.map_err(status)?;
        Ok(Response::new(CreateVolumeResponse {
            volume: Some(Volume {
                capacity_bytes: capacity as i64,
                volume_id: id,
                volume_context: Default::default(),
                content_source: r.volume_content_source,
            }),
        }))
    }

    async fn delete_volume(
        &self,
        req: Request<DeleteVolumeRequest>,
    ) -> Result<Response<DeleteVolumeResponse>, Status> {
        let id = req.into_inner().volume_id;
        if id.is_empty() {
            return Err(Status::invalid_argument("volume_id is required"));
        }
        if valid_id(&id) {
            self.remove(format!("/v1/fs/{id}")).await.map_err(status)?;
        }
        Ok(Response::new(DeleteVolumeResponse {}))
    }

    async fn validate_volume_capabilities(
        &self,
        req: Request<ValidateVolumeCapabilitiesRequest>,
    ) -> Result<Response<ValidateVolumeCapabilitiesResponse>, Status> {
        let r = req.into_inner();
        if r.volume_id.is_empty() {
            return Err(Status::invalid_argument("volume_id is required"));
        }
        if r.volume_capabilities.is_empty() {
            return Err(Status::invalid_argument("volume_capabilities are required"));
        }
        if !valid_id(&r.volume_id) {
            return Err(Status::not_found(format!("volume {}", r.volume_id)));
        }
        self.call(
            Method::GET,
            format!("/v1/fs/{}/statfs", r.volume_id),
            None,
            Retry::Idempotent,
        )
        .await
        .map_err(status)?;
        let resp = match unsupported(&r.volume_capabilities) {
            Some(message) => ValidateVolumeCapabilitiesResponse {
                confirmed: None,
                message,
            },
            None => ValidateVolumeCapabilitiesResponse {
                confirmed: Some(validate_volume_capabilities_response::Confirmed {
                    volume_context: r.volume_context,
                    volume_capabilities: r.volume_capabilities,
                    parameters: r.parameters,
                }),
                message: String::new(),
            },
        };
        Ok(Response::new(resp))
    }

    async fn controller_get_capabilities(
        &self,
        _: Request<ControllerGetCapabilitiesRequest>,
    ) -> Result<Response<ControllerGetCapabilitiesResponse>, Status> {
        use controller_service_capability::rpc::Type;
        Ok(Response::new(ControllerGetCapabilitiesResponse {
            capabilities: vec![
                rpc(Type::CreateDeleteVolume),
                rpc(Type::CreateDeleteSnapshot),
                rpc(Type::CloneVolume),
                rpc(Type::ExpandVolume),
            ],
        }))
    }

    async fn create_snapshot(
        &self,
        req: Request<CreateSnapshotRequest>,
    ) -> Result<Response<CreateSnapshotResponse>, Status> {
        let r = req.into_inner();
        if r.name.is_empty() {
            return Err(Status::invalid_argument("name is required"));
        }
        if r.source_volume_id.is_empty() {
            return Err(Status::invalid_argument("source_volume_id is required"));
        }
        if !valid_id(&r.source_volume_id) {
            return Err(Status::not_found(format!("volume {}", r.source_volume_id)));
        }
        let id = object_id(&r.name);
        self.post(
            format!("/v1/fs/{}/snapshots", r.source_volume_id),
            json!({ "id": id, "name": r.name }),
        )
        .await
        .map_err(status)?;
        let list = self
            .call(
                Method::GET,
                "/v1/fs-snapshots".into(),
                None,
                Retry::Idempotent,
            )
            .await
            .map_err(status)?;
        let list: Value = serde_json::from_slice(&list)
            .map_err(|e| Status::internal(format!("snapshot list: {e}")))?;
        let created_ns = list["snapshots"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["id"] == id.as_str())
            .and_then(|s| s["created_ns"].as_i64())
            .ok_or_else(|| Status::internal(format!("snapshot {id} missing after create")))?;
        Ok(Response::new(CreateSnapshotResponse {
            snapshot: Some(Snapshot {
                size_bytes: 0,
                snapshot_id: id,
                source_volume_id: r.source_volume_id,
                creation_time: Some(Timestamp {
                    seconds: created_ns.div_euclid(1_000_000_000),
                    nanos: created_ns.rem_euclid(1_000_000_000) as i32,
                }),
                ready_to_use: true,
            }),
        }))
    }

    async fn delete_snapshot(
        &self,
        req: Request<DeleteSnapshotRequest>,
    ) -> Result<Response<DeleteSnapshotResponse>, Status> {
        let id = req.into_inner().snapshot_id;
        if id.is_empty() {
            return Err(Status::invalid_argument("snapshot_id is required"));
        }
        if valid_id(&id) {
            self.remove(format!("/v1/fs-snapshots/{id}"))
                .await
                .map_err(status)?;
        }
        Ok(Response::new(DeleteSnapshotResponse {}))
    }

    /// Raises the byte quota to the new size; never lowers it. A volume without a byte quota
    /// (`enforceCapacity: "false"`, or made before quotas) stays thin and reports the new size.
    async fn controller_expand_volume(
        &self,
        req: Request<ControllerExpandVolumeRequest>,
    ) -> Result<Response<ControllerExpandVolumeResponse>, Status> {
        let r = req.into_inner();
        if r.volume_id.is_empty() {
            return Err(Status::invalid_argument("volume_id is required"));
        }
        if !valid_id(&r.volume_id) {
            return Err(Status::not_found(format!("volume {}", r.volume_id)));
        }
        let want = requested(r.capacity_range.as_ref())?;
        if want == 0 {
            return Err(Status::invalid_argument("capacity_range is required"));
        }
        if let Some(why) = r
            .volume_capability
            .as_ref()
            .and_then(|c| unsupported(std::slice::from_ref(c)))
        {
            return Err(Status::invalid_argument(why));
        }
        let quota = self.quota(&r.volume_id).await.map_err(status)?;
        let capacity = match quota["max_bytes"].as_u64() {
            None => want,
            Some(cur) if cur >= want => cur,
            Some(_) => {
                self.set_max_bytes(&r.volume_id, Some(want))
                    .await
                    .map_err(status)?;
                want
            }
        };
        Ok(Response::new(ControllerExpandVolumeResponse {
            capacity_bytes: capacity as i64,
            node_expansion_required: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioner_names_are_kept_and_others_hashed() {
        let pvc = "pvc-0f8e8a43-1c2b-4d5e-9f60-7a8b9c0d1e2f";
        assert_eq!(object_id(pvc), pvc);
        let long = "x".repeat(49);
        let h = object_id(&long);
        assert!(h.starts_with("csi-") && h.len() == 44 && valid_id(&h));
        assert_eq!(h, object_id(&long));
        assert_ne!(object_id("a b"), object_id("a_b"));
        assert!(valid_id(&format!("{}-src", object_id("a b"))));
    }

    #[test]
    fn rejects_block_and_unknown_parameters() {
        let block = VolumeCapability {
            access_type: Some(volume_capability::AccessType::Block(
                volume_capability::BlockVolume {},
            )),
            access_mode: None,
        };
        assert!(unsupported(&[block]).is_some());
        let mut p = std::collections::HashMap::new();
        p.insert("csi.storage.k8s.io/pvc/name".to_string(), "a".to_string());
        assert_eq!(extent_bytes(&p).unwrap(), None);
        p.insert(PARAM_EXTENT_BYTES.into(), "65536".into());
        assert_eq!(extent_bytes(&p).unwrap(), Some(65536));
        p.insert(PARAM_EXTENT_BYTES.into(), "0".into());
        assert!(extent_bytes(&p).is_err());
        p.remove(PARAM_EXTENT_BYTES);
        p.insert("replicas".into(), "3".into());
        assert!(check_params(&p).is_err());
        p.remove("replicas");
        p.insert(PARAM_ENFORCE_CAPACITY.into(), "false".into());
        assert!(check_params(&p).is_ok());
        assert!(!enforce_capacity(&p).unwrap());
        p.insert(PARAM_ENFORCE_CAPACITY.into(), "no".into());
        assert!(enforce_capacity(&p).is_err());
        assert!(enforce_capacity(&Params::new()).unwrap());
    }

    #[test]
    fn capacity_ranges() {
        let r = |required_bytes, limit_bytes| {
            requested(Some(&CapacityRange {
                required_bytes,
                limit_bytes,
            }))
        };
        assert_eq!(requested(None).unwrap(), 0);
        assert_eq!(r(10, 0).unwrap(), 10);
        assert_eq!(r(0, 20).unwrap(), 20);
        assert_eq!(r(10, 20).unwrap(), 10);
        assert_eq!(r(30, 20).unwrap_err().code(), tonic::Code::OutOfRange);
        assert_eq!(r(-1, 0).unwrap_err().code(), tonic::Code::InvalidArgument);
    }
}
