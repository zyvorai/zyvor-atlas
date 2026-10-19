// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! A Mac reached through its `atlas-mac-agent`: the gateway's view of one Mac in a fleet. Discovery
//! is read-only; [`RemoteMacosOps`] runs the guarded lifecycle operations on it, and only if the
//! agent was started with writes allowed.
//! Redirects are off (so the bearer token only ever goes to the configured URL), responses are
//! capped and requests time out; an unreachable agent is an error, never an empty inventory.

use std::time::Duration;

use async_trait::async_trait;
use atlas_api_types::{DiscoveryResult, MetricSample, StorageHealth, StoragePool, StorageVolume};
use atlas_driver_core::{DriverError, StorageDriver};

use crate::ops::{DeviceRow, Lifecycle};
use crate::volumes_of;

const MAX_BYTES: usize = 8 * 1024 * 1024;

pub struct RemoteMacosDriver {
    backend_id: String,
    base_url: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl RemoteMacosDriver {
    /// The lifecycle operations of this Mac (needs an agent started with `ATLAS_MAC_ALLOW_WRITES=1`).
    pub fn ops(&self) -> RemoteMacosOps {
        RemoteMacosOps {
            base_url: self.base_url.clone(),
            token: self.token.clone(),
            client: self.client.clone(),
        }
    }

    /// `ca_pem` adds one trusted root for an agent with a private certificate.
    pub fn new(
        backend_id: impl Into<String>,
        base_url: &str,
        token: Option<String>,
        ca_pem: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<Self, DriverError> {
        let url = reqwest::Url::parse(base_url)
            .map_err(|e| DriverError::Backend(format!("agent URL {base_url:?}: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(DriverError::Backend("agent URL must be http(s)".into()));
        }
        let mut b = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none());
        if let Some(pem) = ca_pem {
            let cert = reqwest::Certificate::from_pem(pem)
                .map_err(|e| DriverError::Backend(format!("agent CA certificate: {e}")))?;
            b = b.add_root_certificate(cert);
        }
        let client = b
            .build()
            .map_err(|e| DriverError::Backend(format!("http client: {e}")))?;
        Ok(Self {
            backend_id: backend_id.into(),
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            client,
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, DriverError> {
        let mut req = self.client.get(format!("{}{path}", self.base_url));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let mut resp = req
            .send()
            .await
            .map_err(|e| DriverError::Unreachable(format!("mac agent {}: {e}", self.base_url)))?;
        if !resp.status().is_success() {
            return Err(DriverError::Backend(format!(
                "mac agent {} answered HTTP {}",
                self.base_url,
                resp.status().as_u16()
            )));
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| DriverError::Unreachable(format!("mac agent response: {e}")))?
        {
            if body.len() + chunk.len() > MAX_BYTES {
                return Err(DriverError::Backend("mac agent response too large".into()));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body)
            .map_err(|e| DriverError::Parse(format!("mac agent {path}: {e}")))
    }
}

#[async_trait]
impl StorageDriver for RemoteMacosDriver {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    async fn discover(&self) -> Result<DiscoveryResult, DriverError> {
        let mut d: DiscoveryResult = self.get("/v1/discover").await?;
        d.cluster.backend_id = self.backend_id.clone();
        Ok(d)
    }

    async fn health(&self) -> Result<StorageHealth, DriverError> {
        Ok(self.discover().await?.health)
    }

    async fn list_pools(&self) -> Result<Vec<StoragePool>, DriverError> {
        Ok(self.discover().await?.pools)
    }

    async fn list_volumes(&self, pool: &str) -> Result<Vec<StorageVolume>, DriverError> {
        Ok(volumes_of(&self.discover().await?, pool))
    }

    async fn metrics(&self) -> Result<Vec<MetricSample>, DriverError> {
        self.get("/v1/metrics").await
    }
}

// ---------------------------------------------------------------------------
// Lifecycle through the agent
// ---------------------------------------------------------------------------

/// Erase, add/delete a volume and eject on another Mac. The agent runs the same refusals against
/// its own live layout; a refusal comes back as `DriverError::Backend("refused: ...")`.
#[derive(Clone)]
pub struct RemoteMacosOps {
    base_url: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl RemoteMacosOps {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, DriverError> {
        let mut req = self.client.request(method, format!("{}{path}", self.base_url));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| DriverError::Unreachable(format!("mac agent {}: {e}", self.base_url)))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| DriverError::Unreachable(format!("mac agent response: {e}")))?;
        let text: String = text.chars().take(2000).collect();
        match status.as_u16() {
            200..=299 => serde_json::from_str(&text)
                .map_err(|e| DriverError::Parse(format!("mac agent {path}: {e}"))),
            409 => Err(DriverError::Backend(format!("refused: {text}"))),
            400 | 403 => Err(DriverError::Backend(format!("invalid: mac agent: {text}"))),
            503 => Err(DriverError::Unreachable(format!("mac agent: {text}"))),
            code => Err(DriverError::Backend(format!("mac agent answered HTTP {code}: {text}"))),
        }
    }

    async fn op(
        &self,
        name: &str,
        mut body: serde_json::Value,
        dry_run: bool,
    ) -> Result<String, DriverError> {
        body["dry_run"] = dry_run.into();
        let v = self.call(reqwest::Method::POST, &format!("/v1/ops/{name}"), Some(body)).await?;
        Ok(v["output"].as_str().unwrap_or_default().to_string())
    }
}

#[async_trait]
impl Lifecycle for RemoteMacosOps {
    async fn devices(&self) -> Result<Vec<DeviceRow>, DriverError> {
        let v = self.call(reqwest::Method::GET, "/v1/devices", None).await?;
        serde_json::from_value(v).map_err(|e| DriverError::Parse(format!("mac agent /v1/devices: {e}")))
    }
    async fn check_erase(&self, whole: &str, name: &str) -> Result<(), DriverError> {
        self.op("erase", serde_json::json!({ "device": whole, "name": name }), true).await.map(drop)
    }
    async fn check_add_volume(&self, container: &str, name: &str) -> Result<(), DriverError> {
        self.op("add-volume", serde_json::json!({ "container": container, "name": name }), true)
            .await
            .map(drop)
    }
    async fn check_delete_volume(&self, volume: &str) -> Result<(), DriverError> {
        self.op("delete-volume", serde_json::json!({ "device": volume }), true).await.map(drop)
    }
    async fn check_resize_container(&self, container: &str, size_bytes: u64) -> Result<(), DriverError> {
        self.op("resize-container", serde_json::json!({ "container": container, "size_bytes": size_bytes }), true)
            .await
            .map(drop)
    }
    async fn resize_container(&self, container: &str, size_bytes: u64) -> Result<String, DriverError> {
        self.op("resize-container", serde_json::json!({ "container": container, "size_bytes": size_bytes }), false)
            .await
    }
    async fn list_local_snapshots(&self) -> Result<Vec<String>, DriverError> {
        let v = self.call(reqwest::Method::GET, "/v1/snapshots", None).await?;
        serde_json::from_value(v).map_err(|e| DriverError::Parse(format!("mac agent /v1/snapshots: {e}")))
    }
    async fn create_local_snapshot(&self) -> Result<String, DriverError> {
        self.op("snapshot-create", serde_json::json!({}), false).await
    }
    async fn check_delete_local_snapshot(&self, date: &str) -> Result<(), DriverError> {
        self.op("snapshot-delete", serde_json::json!({ "date": date }), true).await.map(drop)
    }
    async fn delete_local_snapshot(&self, date: &str) -> Result<String, DriverError> {
        self.op("snapshot-delete", serde_json::json!({ "date": date }), false).await
    }
    async fn check_eject(&self, whole: &str) -> Result<(), DriverError> {
        self.op("eject", serde_json::json!({ "device": whole }), true).await.map(drop)
    }
    async fn erase_disk(&self, whole: &str, name: &str) -> Result<String, DriverError> {
        self.op("erase", serde_json::json!({ "device": whole, "name": name }), false).await
    }
    async fn add_volume(
        &self,
        container: &str,
        name: &str,
        quota: Option<u64>,
        reserve: Option<u64>,
    ) -> Result<String, DriverError> {
        self.op(
            "add-volume",
            serde_json::json!({ "container": container, "name": name,
                                "quota_bytes": quota, "reserve_bytes": reserve }),
            false,
        )
        .await
    }
    async fn delete_volume(&self, volume: &str) -> Result<String, DriverError> {
        self.op("delete-volume", serde_json::json!({ "device": volume }), false).await
    }
    async fn eject(&self, whole: &str) -> Result<String, DriverError> {
        self.op("eject", serde_json::json!({ "device": whole }), false).await
    }
}

// ---------------------------------------------------------------------------
// ATLAS_MACOS_AGENTS
// ---------------------------------------------------------------------------

/// One remote Mac per entry of `ATLAS_MACOS_AGENTS` (`name=https://host:5112,name2=...`), each a
/// backend `bkd_macos_<name>`. `ATLAS_MACOS_AGENT_TOKEN` (or `_TOKEN_FILE`) is the bearer token sent
/// to every agent, `ATLAS_MACOS_AGENT_CA_CERT` an extra trusted root. A bad entry is an error rather
/// than a silently missing Mac. Shared by the gateway (inventory) and job dispatch (lifecycle).
pub fn agents_from_env() -> Result<Vec<RemoteMacosDriver>, String> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let Some(list) = var("ATLAS_MACOS_AGENTS") else {
        return Ok(Vec::new());
    };
    let token = match (var("ATLAS_MACOS_AGENT_TOKEN"), var("ATLAS_MACOS_AGENT_TOKEN_FILE")) {
        (Some(t), _) => Some(t.trim().to_string()),
        (None, Some(f)) => Some(
            std::fs::read_to_string(&f)
                .map_err(|e| format!("ATLAS_MACOS_AGENT_TOKEN_FILE {f}: {e}"))?
                .trim()
                .to_string(),
        ),
        _ => None,
    };
    let ca = match var("ATLAS_MACOS_AGENT_CA_CERT") {
        Some(f) => Some(std::fs::read(&f).map_err(|e| format!("ATLAS_MACOS_AGENT_CA_CERT {f}: {e}"))?),
        None => None,
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let (name, url) = entry
            .split_once('=')
            .ok_or_else(|| format!("ATLAS_MACOS_AGENTS entry {entry:?} is not name=url"))?;
        let ok = !name.is_empty()
            && name.len() <= 40
            && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !ok {
            return Err(format!("ATLAS_MACOS_AGENTS name {name:?} must be lowercase letters, digits or '-'"));
        }
        if !seen.insert(name.to_string()) {
            return Err(format!("ATLAS_MACOS_AGENTS lists {name:?} twice"));
        }
        out.push(
            RemoteMacosDriver::new(
                format!("bkd_macos_{}", name.replace('-', "_")),
                url.trim(),
                token.clone(),
                ca.as_deref(),
                Duration::from_secs(20),
            )
            .map_err(|e| format!("ATLAS_MACOS_AGENTS {name}: {e}"))?,
        );
    }
    Ok(out)
}

/// The lifecycle client for a configured agent backend id, or `None` if no such agent is listed.
pub fn remote_ops_from_env(backend_id: &str) -> Result<Option<RemoteMacosOps>, String> {
    Ok(agents_from_env()?
        .into_iter()
        .find(|d| d.backend_id == backend_id)
        .map(|d| d.ops()))
}
