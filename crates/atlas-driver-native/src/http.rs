// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! The node HTTP API over reqwest, failing over across endpoints.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use atlas_driver_core::DriverError;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{NativeApi, NativeFs, NativeVolume, NodeStatus};

/// How long to keep retrying while every reachable node says it is not the leader (an
/// election in progress). Only 421 is retried this way: nothing was proposed, so a retry cannot
/// apply a mutation twice.
const ELECTION_WAIT: Duration = Duration::from_secs(5);
const ELECTION_RETRY: Duration = Duration::from_millis(100);

/// Where and how to reach a native cluster.
#[derive(Debug, Clone)]
pub struct HttpApiConfig {
    /// Base URLs of the nodes running the metadata role (`https://host:7480`).
    pub endpoints: Vec<String>,
    /// Bearer token (`api_token_file` on the nodes).
    pub token: Option<String>,
    /// PEM CA bundle to trust in addition to the system roots (`http_tls` with a private CA).
    pub ca_pem: Option<Vec<u8>>,
    /// PEM client certificate followed by its key, for nodes with `http_tls.client_ca`.
    pub identity_pem: Option<Vec<u8>>,
    pub timeout: Duration,
}

pub struct HttpApi {
    client: reqwest::Client,
    endpoints: Vec<String>,
    token: Option<String>,
    /// Index of the endpoint that last answered; tried first next time.
    preferred: AtomicUsize,
}

enum Body {
    Json(Value),
    Raw(Vec<u8>),
}

enum Round {
    Done(Vec<u8>),
    /// Some node answered "not the leader".
    NoLeader(String),
    Failed(String),
}

#[derive(Deserialize)]
struct Created {
    id: String,
}

#[derive(Deserialize)]
struct Volumes {
    volumes: Vec<NativeVolume>,
}

#[derive(Deserialize)]
struct Filesystems {
    filesystems: Vec<NativeFs>,
}

impl HttpApi {
    pub fn new(cfg: HttpApiConfig) -> Result<Self, DriverError> {
        let endpoints: Vec<String> = cfg
            .endpoints
            .iter()
            .map(|e| e.trim().trim_end_matches('/').to_string())
            .filter(|e| !e.is_empty())
            .collect();
        if endpoints.is_empty() {
            return Err(DriverError::Backend(
                "atlas-native: at least one endpoint is required".into(),
            ));
        }
        // Below the node's 30 s idle close, so a pooled connection is never reused as it closes.
        let mut builder = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .pool_idle_timeout(std::time::Duration::from_secs(20));
        if let Some(pem) = &cfg.ca_pem {
            let certs = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|e| DriverError::Backend(format!("atlas-native CA bundle: {e}")))?;
            if certs.is_empty() {
                return Err(DriverError::Backend(
                    "atlas-native CA bundle has no certificate".into(),
                ));
            }
            builder = builder.tls_certs_merge(certs);
        }
        if let Some(pem) = &cfg.identity_pem {
            let id = reqwest::Identity::from_pem(pem).map_err(|e| {
                DriverError::Backend(format!("atlas-native client certificate: {e}"))
            })?;
            builder = builder.identity(id);
        }
        let client = builder
            .build()
            .map_err(|e| DriverError::Backend(format!("atlas-native HTTP client: {e}")))?;
        Ok(Self {
            client,
            endpoints,
            token: cfg.token.filter(|t| !t.is_empty()),
            preferred: AtomicUsize::new(0),
        })
    }

    /// Sends to each endpoint in turn, starting with the last one that answered, until one
    /// answers with something other than "not the leader" or "try again"; repeats the round for
    /// up to [`ELECTION_WAIT`] while some node answers "not the leader".
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Body>,
    ) -> Result<Vec<u8>, DriverError> {
        let deadline = Instant::now() + ELECTION_WAIT;
        let mut ambiguous = false;
        loop {
            match self
                .round(&method, path, body.as_ref(), &mut ambiguous)
                .await?
            {
                Round::Done(b) => return Ok(b),
                Round::NoLeader(_) if Instant::now() < deadline => {
                    tokio::time::sleep(ELECTION_RETRY).await;
                }
                Round::NoLeader(last) | Round::Failed(last) => {
                    return Err(DriverError::Unreachable(format!(
                        "no atlas-native endpoint accepted {method} {path} (last: {last})"
                    )));
                }
            }
        }
    }

    async fn round(
        &self,
        method: &Method,
        path: &str,
        body: Option<&Body>,
        ambiguous: &mut bool,
    ) -> Result<Round, DriverError> {
        let n = self.endpoints.len();
        let start = self.preferred.load(Ordering::Relaxed) % n;
        let mut last = String::new();
        let mut not_leader = false;
        for k in 0..n {
            let i = (start + k) % n;
            let url = format!("{}{path}", self.endpoints[i]);
            let mut req = self.client.request(method.clone(), &url);
            if let Some(t) = &self.token {
                req = req.bearer_auth(t);
            }
            req = match body {
                Some(Body::Json(v)) => req.json(v),
                Some(Body::Raw(b)) => req
                    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                    .body(b.clone()),
                None => req,
            };
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    *ambiguous |= e.is_timeout();
                    last = format!("{url}: {e}");
                    continue;
                }
            };
            let status = resp.status();
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| DriverError::Unreachable(format!("{url}: {e}")))?
                .to_vec();
            if status.is_success() {
                self.preferred.store(i, Ordering::Relaxed);
                return Ok(Round::Done(bytes));
            }
            let msg = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).trim().to_string());
            match status {
                StatusCode::MISDIRECTED_REQUEST | StatusCode::SERVICE_UNAVAILABLE => {
                    not_leader |= status == StatusCode::MISDIRECTED_REQUEST;
                    // A 503 can follow a proposal that still commits.
                    *ambiguous |= status == StatusCode::SERVICE_UNAVAILABLE;
                    last = format!("{url}: {status} {msg}");
                }
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                    return Err(DriverError::Unreachable(format!(
                        "atlas-native refused the credentials: {status} {msg}"
                    )));
                }
                // An earlier attempt may have deleted it already.
                StatusCode::NOT_FOUND if *method == Method::DELETE && *ambiguous => {
                    return Ok(Round::Done(Vec::new()));
                }
                StatusCode::NOT_FOUND => {
                    return Err(DriverError::Backend(format!("not found: {msg}")));
                }
                StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE => {
                    return Err(DriverError::Backend(format!("invalid: {msg}")));
                }
                _ => return Err(DriverError::Backend(format!("{status}: {msg}"))),
            }
        }
        Ok(if not_leader {
            Round::NoLeader(last)
        } else {
            Round::Failed(last)
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, DriverError> {
        let b = self.call(Method::GET, path, None).await?;
        serde_json::from_slice(&b).map_err(|e| DriverError::Parse(format!("{path}: {e}")))
    }

    async fn post_created(&self, path: &str, body: Value) -> Result<String, DriverError> {
        let b = self
            .call(Method::POST, path, Some(Body::Json(body)))
            .await?;
        serde_json::from_slice::<Created>(&b)
            .map(|c| c.id)
            .map_err(|e| DriverError::Parse(format!("{path}: {e}")))
    }

    /// `POST …/lock {retain_secs}` → the Unix second the lock expires at.
    async fn lock_call(&self, path: &str, retain_secs: u64) -> Result<u64, DriverError> {
        let b = self
            .call(
                Method::POST,
                path,
                Some(Body::Json(json!({ "retain_secs": retain_secs }))),
            )
            .await?;
        serde_json::from_slice::<Value>(&b)
            .ok()
            .and_then(|v| v["retain_until_unix"].as_u64())
            .ok_or_else(|| DriverError::Parse(format!("{path}: missing retain_until_unix")))
    }
}

/// Native ids are server-generated UUIDs; anything else is refused before it reaches a path.
fn path_id(id: &str) -> Result<&str, DriverError> {
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        Ok(id)
    } else {
        Err(DriverError::Backend(format!(
            "invalid atlas-native id {id:?}"
        )))
    }
}

#[async_trait]
impl NativeApi for HttpApi {
    /// Waits up to [`ELECTION_WAIT`] for a metadata leader, so an election in progress isn't
    /// reported as a leaderless (critical) cluster.
    async fn status(&self) -> Result<NodeStatus, DriverError> {
        let deadline = Instant::now() + ELECTION_WAIT;
        loop {
            let s: NodeStatus = self.get("/v1/status").await?;
            let electing = s.leaderless_group().is_some();
            if !electing || Instant::now() >= deadline {
                return Ok(s);
            }
            tokio::time::sleep(ELECTION_RETRY).await;
        }
    }

    async fn volumes(&self) -> Result<Vec<NativeVolume>, DriverError> {
        Ok(self.get::<Volumes>("/v1/volumes").await?.volumes)
    }

    async fn create_volume(
        &self,
        id: &str,
        name: &str,
        size_bytes: u64,
    ) -> Result<String, DriverError> {
        self.post_created(
            "/v1/volumes",
            json!({ "id": path_id(id)?, "name": name, "size_bytes": size_bytes }),
        )
        .await
    }

    async fn delete_volume(&self, id: &str) -> Result<(), DriverError> {
        let path = format!("/v1/volumes/{}", path_id(id)?);
        self.call(Method::DELETE, &path, None).await.map(drop)
    }

    async fn create_snapshot(
        &self,
        id: &str,
        volume_id: &str,
        name: &str,
    ) -> Result<String, DriverError> {
        let path = format!("/v1/volumes/{}/snapshots", path_id(volume_id)?);
        self.post_created(&path, json!({ "id": path_id(id)?, "name": name }))
            .await
    }

    async fn resize_volume(&self, id: &str, size_bytes: u64) -> Result<(), DriverError> {
        let path = format!("/v1/volumes/{}/resize", path_id(id)?);
        self.call(
            Method::POST,
            &path,
            Some(Body::Json(json!({ "size_bytes": size_bytes }))),
        )
        .await
        .map(drop)
    }

    async fn clone_snapshot(
        &self,
        id: &str,
        snapshot_id: &str,
        name: &str,
        size_bytes: Option<u64>,
    ) -> Result<String, DriverError> {
        let path = format!("/v1/snapshots/{}/clone", path_id(snapshot_id)?);
        self.post_created(
            &path,
            json!({ "id": path_id(id)?, "name": name, "size_bytes": size_bytes }),
        )
        .await
    }

    async fn delete_snapshot(&self, id: &str) -> Result<(), DriverError> {
        let path = format!("/v1/snapshots/{}", path_id(id)?);
        self.call(Method::DELETE, &path, None).await.map(drop)
    }

    async fn read(&self, volume_id: &str, offset: u64, len: u64) -> Result<Vec<u8>, DriverError> {
        let path = format!(
            "/v1/volumes/{}/data?offset={offset}&len={len}",
            path_id(volume_id)?
        );
        self.call(Method::GET, &path, None).await
    }

    async fn write(&self, volume_id: &str, offset: u64, data: Vec<u8>) -> Result<(), DriverError> {
        // Rewriting the same bytes is harmless, so the usual failover/retry applies.
        let path = format!("/v1/volumes/{}/data?offset={offset}", path_id(volume_id)?);
        self.call(Method::PUT, &path, Some(Body::Raw(data)))
            .await
            .map(drop)
    }

    async fn filesystems(&self) -> Result<Vec<NativeFs>, DriverError> {
        match self.get::<Filesystems>("/v1/fs").await {
            Ok(f) => Ok(f.filesystems),
            // Nodes from before the file namespace have no `/v1/fs`.
            Err(DriverError::Backend(m)) if m.starts_with("not found") => Ok(vec![]),
            Err(e) => Err(e),
        }
    }

    async fn create_fs(&self, id: &str, name: &str) -> Result<String, DriverError> {
        self.post_created("/v1/fs", json!({ "id": path_id(id)?, "name": name }))
            .await
    }

    async fn delete_fs(&self, id: &str) -> Result<(), DriverError> {
        let path = format!("/v1/fs/{}", path_id(id)?);
        self.call(Method::DELETE, &path, None).await.map(drop)
    }

    async fn create_fs_snapshot(
        &self,
        id: &str,
        fs_id: &str,
        name: &str,
    ) -> Result<String, DriverError> {
        let path = format!("/v1/fs/{}/snapshots", path_id(fs_id)?);
        self.post_created(&path, json!({ "id": path_id(id)?, "name": name }))
            .await
    }

    async fn clone_fs_snapshot(
        &self,
        id: &str,
        snapshot_id: &str,
        name: &str,
    ) -> Result<String, DriverError> {
        let path = format!("/v1/fs-snapshots/{}/clone", path_id(snapshot_id)?);
        self.post_created(&path, json!({ "id": path_id(id)?, "name": name }))
            .await
    }

    async fn delete_fs_snapshot(&self, id: &str) -> Result<(), DriverError> {
        let path = format!("/v1/fs-snapshots/{}", path_id(id)?);
        self.call(Method::DELETE, &path, None).await.map(drop)
    }

    async fn lock_snapshot(&self, id: &str, retain_secs: u64) -> Result<u64, DriverError> {
        let path = format!("/v1/snapshots/{}/lock", path_id(id)?);
        self.lock_call(&path, retain_secs).await
    }

    async fn lock_fs_snapshot(&self, id: &str, retain_secs: u64) -> Result<u64, DriverError> {
        let path = format!("/v1/fs-snapshots/{}/lock", path_id(id)?);
        self.lock_call(&path, retain_secs).await
    }
}
