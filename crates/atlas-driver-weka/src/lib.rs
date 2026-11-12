// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! WEKA through the REST API (v2): `POST /login`, `GET /cluster`, `GET /fileSystems`.
//! Filesystems become Atlas `filesystem` volumes in one pool. `create_volume` posts
//! `/fileSystems` and `delete_volume` deletes `/fileSystems/{id}`. Fake mode keeps the same
//! list in memory and mutates it. Fields the API doesn't return stay unknown.

use std::time::Duration;

use async_trait::async_trait;
use atlas_api_types::{
    CreateVolumeRequest, CreateVolumeResult, DeleteVolumeRequest, DiscoveryResult, Health,
    MetricSample, StorageCluster, StorageHealth, StoragePool, StorageVolume, VolumeKind,
};
use atlas_driver_core::{DriverError, StorageDriver};
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// Connection settings for a real WEKA cluster.
#[derive(Clone)]
pub struct WekaConfig {
    /// API base, e.g. `https://weka01:14000/api/v2`.
    pub endpoint: String,
    pub username: String,
    pub password: String,
    pub org: Option<String>,
    pub ca_pem: Option<Vec<u8>>,
    pub timeout: Duration,
}

struct Conn {
    cfg: WekaConfig,
    client: reqwest::Client,
    token: Mutex<Option<String>>,
}

struct Fake {
    filesystems: Mutex<Vec<Value>>,
}

enum Mode {
    Fake(Fake),
    Real(Box<Conn>),
}

pub struct WekaDriver {
    backend_id: String,
    mode: Mode,
}

impl WekaDriver {
    /// Fixture-only driver: a small two-filesystem cluster, no network.
    /// `create_volume` and `delete_volume` change that in-memory list.
    pub fn fake(backend_id: impl Into<String>) -> Self {
        Self {
            backend_id: backend_id.into(),
            mode: Mode::Fake(Fake {
                filesystems: Mutex::new(fixture_filesystems()),
            }),
        }
    }

    pub fn real(backend_id: impl Into<String>, cfg: WekaConfig) -> Result<Self, DriverError> {
        let mut builder = reqwest::Client::builder().timeout(cfg.timeout);
        if let Some(pem) = &cfg.ca_pem {
            let certs = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|e| DriverError::Backend(format!("WEKA CA bundle: {e}")))?;
            if certs.is_empty() {
                return Err(DriverError::Backend(
                    "WEKA CA bundle has no certificate".into(),
                ));
            }
            builder = builder.tls_certs_merge(certs);
        }
        let client = builder
            .build()
            .map_err(|e| DriverError::Backend(format!("WEKA HTTP client: {e}")))?;
        let cfg = WekaConfig {
            endpoint: cfg.endpoint.trim_end_matches('/').to_string(),
            ..cfg
        };
        Ok(Self {
            backend_id: backend_id.into(),
            mode: Mode::Real(Box::new(Conn {
                cfg,
                client,
                token: Mutex::new(None),
            })),
        })
    }

    async fn snapshot(&self) -> Result<DiscoveryResult, DriverError> {
        match &self.mode {
            Mode::Fake(fake) => {
                let filesystems = fake.filesystems.lock().await.clone();
                Ok(build_discovery(
                    &self.backend_id,
                    &fixture_cluster(),
                    &filesystems,
                ))
            }
            Mode::Real(c) => {
                let cluster = get(&c.cfg, &c.client, &c.token, "/cluster").await?;
                let filesystems = get(&c.cfg, &c.client, &c.token, "/fileSystems").await?;
                let filesystems = filesystems.as_array().cloned().unwrap_or_default();
                Ok(build_discovery(&self.backend_id, &cluster, &filesystems))
            }
        }
    }
}

async fn login(cfg: &WekaConfig, client: &reqwest::Client) -> Result<String, DriverError> {
    let mut body = json!({ "username": cfg.username, "password": cfg.password });
    if let Some(org) = &cfg.org {
        body["org"] = json!(org);
    }
    let resp = client
        .post(format!("{}/login", cfg.endpoint))
        .json(&body)
        .send()
        .await
        .map_err(|e| DriverError::Unreachable(format!("WEKA login at {}: {e}", cfg.endpoint)))?;
    let status = resp.status();
    let v: Value = resp
        .json()
        .await
        .map_err(|e| DriverError::Parse(format!("WEKA login response: {e}")))?;
    if !status.is_success() {
        return Err(DriverError::Backend(format!("WEKA login: HTTP {status}")));
    }
    data(&v)
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| DriverError::Parse("WEKA login: no access_token".into()))
}

/// One REST call with a cached bearer token; logs in again once when the token is rejected.
/// DELETE of a missing filesystem is success, so a retried delete does not fail.
async fn request(
    cfg: &WekaConfig,
    client: &reqwest::Client,
    token: &Mutex<Option<String>>,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, DriverError> {
    let mut guard = token.lock().await;
    for attempt in 0..2 {
        let bearer = match guard.as_ref() {
            Some(t) => t.clone(),
            None => {
                let t = login(cfg, client).await?;
                *guard = Some(t.clone());
                t
            }
        };
        let mut req = client
            .request(method.clone(), format!("{}{path}", cfg.endpoint))
            .bearer_auth(&bearer);
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| DriverError::Unreachable(format!("WEKA {method} {path}: {e}")))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
            *guard = None;
            continue;
        }
        if status == reqwest::StatusCode::NOT_FOUND && method == reqwest::Method::DELETE {
            return Ok(Value::Null);
        }
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            let detail: String = detail.chars().take(200).collect();
            return Err(DriverError::Backend(format!(
                "WEKA {method} {path}: HTTP {status} {detail}"
            )));
        }
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| DriverError::Parse(format!("WEKA {method} {path}: {e}")))?;
        return Ok(data(&v).clone());
    }
    Err(DriverError::Backend(format!(
        "WEKA {method} {path}: token rejected after login"
    )))
}

async fn get(
    cfg: &WekaConfig,
    client: &reqwest::Client,
    token: &Mutex<Option<String>>,
    path: &str,
) -> Result<Value, DriverError> {
    request(cfg, client, token, reqwest::Method::GET, path, None).await
}

/// WEKA wraps responses as `{"data": ...}`.
fn data(v: &Value) -> &Value {
    v.get("data").unwrap_or(v)
}

fn number(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    v.as_i64()
        .or_else(|| v.as_f64().map(|f| f as i64))
        .or_else(|| v.as_str()?.parse().ok())
}

fn fs_health(fs: &Value) -> Health {
    match (
        fs.get("status").and_then(Value::as_str),
        fs.get("is_ready").and_then(Value::as_bool),
    ) {
        (Some("READY"), _) | (None, Some(true)) => Health::Ok,
        (Some("CREATING" | "REMOVING" | "DOWNLOADING"), _) => Health::Warn,
        (Some(_), _) | (None, Some(false)) => Health::Critical,
        (None, None) => Health::Unknown,
    }
}

fn build_discovery(backend_id: &str, cluster: &Value, filesystems: &[Value]) -> DiscoveryResult {
    let cluster_id = format!("cls_{backend_id}");
    let pool_id = format!("pool_{backend_id}");
    let capacity = cluster.get("capacity");
    let total = number(capacity.and_then(|c| c.get("total_bytes")));
    let unprovisioned = number(capacity.and_then(|c| c.get("unprovisioned_bytes")));
    let used = match (total, unprovisioned) {
        (Some(t), Some(u)) => Some(t.saturating_sub(u)),
        _ => None,
    };
    let cluster_status = match cluster.get("status").and_then(Value::as_str) {
        Some("OK") => Health::Ok,
        Some(_) => Health::Warn,
        None => Health::Unknown,
    };
    let volumes: Vec<StorageVolume> = filesystems
        .iter()
        .filter_map(|fs| {
            let name = fs.get("name").and_then(Value::as_str)?;
            let uid = fs
                .get("uid")
                .or_else(|| fs.get("id"))
                .and_then(Value::as_str);
            let health = fs_health(fs);
            Some(StorageVolume {
                id: format!("vol_{backend_id}_{name}"),
                cluster_id: Some(cluster_id.clone()),
                pool_id: Some(pool_id.clone()),
                name: name.into(),
                kind: VolumeKind::Filesystem,
                backend_native_id: Some(format!("weka-fs:{}", uid.unwrap_or(name))),
                size_bytes: number(fs.get("total_budget")).unwrap_or(0),
                used_bytes: number(fs.get("used_total")),
                state: fs
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_lowercase)
                    .unwrap_or_else(|| "unknown".into()),
                health,
                kubernetes_namespace: None,
                pvc_name: None,
                storage_class_name: None,
            })
        })
        .collect();
    let degraded = volumes
        .iter()
        .filter(|v| matches!(v.health, Health::Warn | Health::Critical))
        .count();
    let status = if volumes.iter().any(|v| v.health == Health::Critical) {
        Health::Critical
    } else if degraded > 0 && cluster_status == Health::Ok {
        Health::Warn
    } else {
        cluster_status
    };
    let name = cluster
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("weka");
    DiscoveryResult {
        cluster: StorageCluster {
            id: cluster_id.clone(),
            backend_id: backend_id.into(),
            name: format!("weka/{name}"),
            native_fsid: cluster
                .get("guid")
                .and_then(Value::as_str)
                .map(str::to_owned),
            health: status,
            raw_capacity_bytes: total,
            used_capacity_bytes: used,
            available_capacity_bytes: unprovisioned,
        },
        pools: vec![StoragePool {
            id: pool_id,
            cluster_id,
            name: "weka".into(),
            kind: "weka".into(),
            device_class: None,
            replica_size: None,
            used_bytes: used,
            max_bytes: total,
            health: status,
        }],
        osds: vec![],
        health: StorageHealth {
            status,
            summary: format!("WEKA {name}: {} filesystem(s)", volumes.len()),
            raw_capacity_bytes: total,
            used_capacity_bytes: used,
            available_capacity_bytes: unprovisioned,
            recovering: cluster
                .get("rebuild")
                .and_then(|r| r.get("progressPercent"))
                .is_some_and(|p| p.as_f64().unwrap_or(100.0) < 100.0),
            degraded_objects: degraded as i64,
        },
        volumes,
    }
}

fn fixture_cluster() -> Value {
    json!({
        "name": "weka-demo", "guid": "00000000-0000-4000-8000-00000000weka", "status": "OK",
        "capacity": { "total_bytes": 109_951_162_777_600_i64, "unprovisioned_bytes": 43_980_465_111_040_i64 }
    })
}

fn valid_fs_key(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
}

/// Atlas pool `weka` is the synthetic discovery pool, not a WEKA filesystem group.
fn filesystem_group(pool: Option<&str>) -> Result<String, DriverError> {
    match pool
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "weka")
    {
        Some(group) if valid_fs_key(group) => Ok(group.to_string()),
        Some(group) => Err(DriverError::Backend(format!(
            "invalid: WEKA group {group:?}"
        ))),
        None => Ok("default".into()),
    }
}

fn prepare_filesystem(req: &CreateVolumeRequest) -> Result<(String, String, i64), DriverError> {
    if req.kind != VolumeKind::Filesystem {
        return Err(DriverError::Backend(
            "invalid: WEKA volumes are filesystems".into(),
        ));
    }
    let name = req.name.trim();
    if !valid_fs_key(name) {
        return Err(DriverError::Backend(format!(
            "invalid: filesystem name {name:?} must be 1-128 ASCII letters, digits, '-', '_' or '.'"
        )));
    }
    if req.size_bytes <= 0 {
        return Err(DriverError::Backend(
            "invalid: size_bytes must be > 0".into(),
        ));
    }
    Ok((
        name.to_string(),
        filesystem_group(req.pool.as_deref())?,
        req.size_bytes,
    ))
}

fn filesystem_target<'a>(backend_id: &str, id: &'a str) -> Result<&'a str, DriverError> {
    let id = id.trim();
    let key = if let Some(rest) = id.strip_prefix("weka-fs:") {
        rest
    } else {
        let prefix = format!("vol_{backend_id}_");
        id.strip_prefix(&prefix).unwrap_or(id)
    };
    if valid_fs_key(key) {
        Ok(key)
    } else {
        Err(DriverError::Backend(format!(
            "invalid: WEKA filesystem id {id:?}"
        )))
    }
}

fn filesystem_create_body(name: &str, group: &str, size_bytes: i64) -> Value {
    json!({
        "name": name,
        "group_name": group,
        "total_capacity": size_bytes,
    })
}

fn created_filesystem(
    backend_id: &str,
    requested_name: &str,
    created: &Value,
) -> CreateVolumeResult {
    let name = created
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(requested_name);
    let uid = created
        .get("uid")
        .or_else(|| created.get("id"))
        .and_then(Value::as_str)
        .unwrap_or(name);
    CreateVolumeResult {
        volume_id: format!("vol_{backend_id}_{name}"),
        backend_native_id: format!("weka-fs:{uid}"),
    }
}

async fn fake_create(
    fake: &Fake,
    backend_id: &str,
    name: &str,
    size_bytes: i64,
) -> Result<CreateVolumeResult, DriverError> {
    let mut list = fake.filesystems.lock().await;
    if list
        .iter()
        .any(|fs| fs.get("name").and_then(Value::as_str) == Some(name))
    {
        return Err(DriverError::Backend(format!(
            "filesystem {name} already exists"
        )));
    }
    let uid = format!("fs-uid-{name}");
    let fs = json!({
        "name": name,
        "uid": uid,
        "status": "READY",
        "total_budget": size_bytes,
    });
    list.push(fs.clone());
    Ok(created_filesystem(backend_id, name, &fs))
}

async fn fake_delete(fake: &Fake, key: &str) -> Result<(), DriverError> {
    let mut list = fake.filesystems.lock().await;
    list.retain(|fs| {
        fs.get("uid").and_then(Value::as_str) != Some(key)
            && fs.get("name").and_then(Value::as_str) != Some(key)
    });
    Ok(())
}

fn fixture_filesystems() -> Vec<Value> {
    vec![
        json!({ "name": "training-data", "uid": "fs-uid-train", "status": "READY",
                "total_budget": 54_975_581_388_800_i64, "used_total": 41_231_686_041_600_i64 }),
        json!({ "name": "checkpoints", "uid": "fs-uid-ckpt", "status": "READY",
                "total_budget": 10_995_116_277_760_i64, "used_total": 2_199_023_255_552_i64 }),
    ]
}

#[async_trait]
impl StorageDriver for WekaDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }
    fn is_fixture(&self) -> bool {
        matches!(self.mode, Mode::Fake(_))
    }
    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        self.snapshot().await
    }
    async fn health(&self) -> Result<StorageHealth, DriverError> {
        Ok(self.snapshot().await?.health)
    }
    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        Ok(self.snapshot().await?.pools)
    }
    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        if pool != "weka" {
            return Ok(vec![]);
        }
        Ok(self.snapshot().await?.volumes)
    }
    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        let snapshot = self.snapshot().await?;
        let sample = |name: &str, value: f64| MetricSample {
            name: name.into(),
            value,
            labels: Default::default(),
        };
        let mut result = vec![sample(
            "weka_filesystems_total",
            snapshot.volumes.len() as f64,
        )];
        if let Some(capacity) = snapshot.cluster.raw_capacity_bytes {
            result.push(sample("weka_capacity_bytes", capacity as f64));
        }
        if let Some(used) = snapshot.cluster.used_capacity_bytes {
            result.push(sample("weka_used_bytes", used as f64));
        }
        Ok(result)
    }

    async fn create_volume(
        &self,
        req: CreateVolumeRequest,
    ) -> Result<CreateVolumeResult, DriverError> {
        let (name, group, size) = prepare_filesystem(&req)?;
        match &self.mode {
            Mode::Fake(fake) => fake_create(fake, &self.backend_id, &name, size).await,
            Mode::Real(c) => {
                let body = filesystem_create_body(&name, &group, size);
                let created = request(
                    &c.cfg,
                    &c.client,
                    &c.token,
                    reqwest::Method::POST,
                    "/fileSystems",
                    Some(&body),
                )
                .await?;
                Ok(created_filesystem(&self.backend_id, &name, &created))
            }
        }
    }

    async fn delete_volume(&self, req: DeleteVolumeRequest) -> Result<(), DriverError> {
        let key = filesystem_target(&self.backend_id, &req.volume_id)?;
        match &self.mode {
            Mode::Fake(fake) => fake_delete(fake, key).await,
            Mode::Real(c) => {
                request(
                    &c.cfg,
                    &c.client,
                    &c.token,
                    reqwest::Method::DELETE,
                    &format!("/fileSystems/{key}"),
                    None,
                )
                .await?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Path, Request},
        http::StatusCode,
        routing::{delete, get, post},
        Json, Router,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn maps_capacity_and_filesystems_without_inventing_missing_values() {
        let r = build_discovery("bkd_weka", &fixture_cluster(), &fixture_filesystems());
        assert_eq!(r.cluster.raw_capacity_bytes, Some(109_951_162_777_600));
        assert_eq!(
            r.cluster.used_capacity_bytes,
            Some(109_951_162_777_600 - 43_980_465_111_040)
        );
        assert_eq!(r.health.status, Health::Ok);
        assert_eq!(r.volumes.len(), 2);
        assert_eq!(r.volumes[0].kind, VolumeKind::Filesystem);
        assert_eq!(
            r.volumes[0].backend_native_id.as_deref(),
            Some("weka-fs:fs-uid-train")
        );
        assert_eq!(r.volumes[0].state, "ready");

        let bare = build_discovery("bkd_weka", &json!({}), &[json!({"name": "x"})]);
        assert_eq!(bare.cluster.raw_capacity_bytes, None);
        assert_eq!(bare.cluster.used_capacity_bytes, None);
        assert_eq!(bare.volumes[0].used_bytes, None);
        assert_eq!(bare.volumes[0].health, Health::Unknown);
        assert_eq!(bare.health.status, Health::Unknown);

        let bad = build_discovery(
            "bkd_weka",
            &fixture_cluster(),
            &[json!({"name": "y", "status": "ERROR"})],
        );
        assert_eq!(bad.health.status, Health::Critical);
    }

    #[tokio::test]
    async fn real_mode_logs_in_and_relogs_on_401() {
        let logins = Arc::new(AtomicUsize::new(0));
        let gets = Arc::new(AtomicUsize::new(0));
        let (l, g) = (logins.clone(), gets.clone());
        let auth = move |req: &Request| {
            req.headers()
                .get("authorization")
                .and_then(|h| h.to_str().ok())
                .map(str::to_owned)
        };
        let app = Router::new()
            .route(
                "/api/v2/login",
                post(move |Json(body): Json<Value>| {
                    let n = l.fetch_add(1, Ordering::SeqCst) + 1;
                    async move {
                        assert_eq!(body["username"], "atlas");
                        Json(json!({"data": {"access_token": format!("tok{n}")}}))
                    }
                }),
            )
            .route(
                "/api/v2/cluster",
                get(move |req: Request| {
                    let n = g.fetch_add(1, Ordering::SeqCst);
                    let a = auth(&req);
                    async move {
                        // The first token is rejected once to exercise the re-login path.
                        if n == 0 || a.as_deref() != Some("Bearer tok2") {
                            return Err(StatusCode::UNAUTHORIZED);
                        }
                        Ok(Json(json!({"data": fixture_cluster()})))
                    }
                }),
            )
            .route(
                "/api/v2/fileSystems",
                get(|| async { Json(json!({"data": fixture_filesystems()})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let d = WekaDriver::real(
            "bkd_weka",
            WekaConfig {
                endpoint: format!("http://{addr}/api/v2/"),
                username: "atlas".into(),
                password: "pw".into(),
                org: None,
                ca_pem: None,
                timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        assert!(!d.is_fixture());
        let r = d.discover().await.unwrap();
        assert_eq!(r.volumes.len(), 2);
        assert_eq!(logins.load(Ordering::SeqCst), 2);

        let down = WekaDriver::real(
            "bkd_weka",
            WekaConfig {
                endpoint: "http://127.0.0.1:1/api/v2".into(),
                username: "atlas".into(),
                password: "pw".into(),
                org: None,
                ca_pem: None,
                timeout: Duration::from_secs(2),
            },
        )
        .unwrap();
        assert!(matches!(
            down.discover().await,
            Err(DriverError::Unreachable(_))
        ));
    }

    fn fs_req(name: &str, size: i64, pool: Option<&str>) -> CreateVolumeRequest {
        CreateVolumeRequest {
            tenant_id: "t1".into(),
            name: name.into(),
            size_bytes: size,
            kind: VolumeKind::Filesystem,
            policy: None,
            pool: pool.map(str::to_owned),
            owner: None,
            kubernetes: None,
            longhorn: None,
        }
    }

    #[tokio::test]
    async fn fake_create_and_delete_mutate_the_filesystem_list() {
        let d = WekaDriver::fake("bkd_weka");
        assert!(d.is_fixture());
        let before = d.list_volumes("weka").await.unwrap().len();
        let created = d
            .create_volume(fs_req("scratch", 1_073_741_824, Some("weka")))
            .await
            .unwrap();
        assert_eq!(created.volume_id, "vol_bkd_weka_scratch");
        assert_eq!(created.backend_native_id, "weka-fs:fs-uid-scratch");
        let vols = d.list_volumes("weka").await.unwrap();
        assert_eq!(vols.len(), before + 1);
        let scratch = vols.iter().find(|v| v.name == "scratch").unwrap();
        assert_eq!(scratch.size_bytes, 1_073_741_824);
        assert_eq!(scratch.kind, VolumeKind::Filesystem);
        assert!(scratch.used_bytes.is_none());
        assert!(d
            .create_volume(fs_req("scratch", 1_073_741_824, None))
            .await
            .is_err());
        let mut block = fs_req("other", 10, None);
        block.kind = VolumeKind::Block;
        assert!(d.create_volume(block).await.is_err());
        assert!(d.create_volume(fs_req("empty", 0, None)).await.is_err());

        d.delete_volume(DeleteVolumeRequest {
            volume_id: created.backend_native_id.clone(),
        })
        .await
        .unwrap();
        assert_eq!(d.list_volumes("weka").await.unwrap().len(), before);
        d.delete_volume(DeleteVolumeRequest {
            volume_id: "weka-fs:missing".into(),
        })
        .await
        .unwrap();
        assert!(d
            .delete_volume(DeleteVolumeRequest {
                volume_id: "weka-fs:../x".into()
            })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn real_mode_creates_and_deletes_filesystems() {
        let filesystems = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let store = filesystems.clone();
        let app = Router::new()
            .route(
                "/api/v2/login",
                post(|| async { Json(json!({"data": {"access_token": "tok1"}})) }),
            )
            .route(
                "/api/v2/cluster",
                get(|| async { Json(json!({"data": {"name": "weka-demo", "status": "OK"}})) }),
            )
            .route(
                "/api/v2/fileSystems",
                post(move |Json(body): Json<Value>| {
                    let store = store.clone();
                    async move {
                        assert_eq!(body["group_name"], "gold");
                        assert_eq!(body["name"], "scratch");
                        assert_eq!(body["total_capacity"], 4096);
                        let fs = json!({
                            "name": "scratch", "uid": "uid-scratch", "status": "CREATING",
                            "total_budget": body["total_capacity"],
                        });
                        store.lock().unwrap().push(fs.clone());
                        Json(json!({"data": fs}))
                    }
                })
                .get({
                    let filesystems = filesystems.clone();
                    move || {
                        let filesystems = filesystems.clone();
                        async move { Json(json!({"data": filesystems.lock().unwrap().clone()})) }
                    }
                }),
            )
            .route(
                "/api/v2/fileSystems/{id}",
                delete({
                    let filesystems = filesystems.clone();
                    move |Path(id): Path<String>| {
                        let filesystems = filesystems.clone();
                        async move {
                            let mut list = filesystems.lock().unwrap();
                            let before = list.len();
                            list.retain(|fs| {
                                fs.get("uid").and_then(Value::as_str) != Some(id.as_str())
                                    && fs.get("name").and_then(Value::as_str) != Some(id.as_str())
                            });
                            if list.len() == before {
                                StatusCode::NOT_FOUND
                            } else {
                                StatusCode::NO_CONTENT
                            }
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let d = WekaDriver::real(
            "bkd_weka",
            WekaConfig {
                endpoint: format!("http://{addr}/api/v2"),
                username: "atlas".into(),
                password: "pw".into(),
                org: None,
                ca_pem: None,
                timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        let created = d
            .create_volume(fs_req("scratch", 4096, Some("gold")))
            .await
            .unwrap();
        assert_eq!(created.backend_native_id, "weka-fs:uid-scratch");
        assert_eq!(created.volume_id, "vol_bkd_weka_scratch");
        let listed = d.list_volumes("weka").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].size_bytes, 4096);
        d.delete_volume(DeleteVolumeRequest {
            volume_id: created.backend_native_id,
        })
        .await
        .unwrap();
        assert!(d.list_volumes("weka").await.unwrap().is_empty());
        d.delete_volume(DeleteVolumeRequest {
            volume_id: "weka-fs:uid-scratch".into(),
        })
        .await
        .unwrap();
    }
}
