// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! A Mac reached through its `atlas-mac-agent`: the gateway's view of one Mac in a fleet. Read-only.
//! Redirects are off (so the bearer token only ever goes to the configured URL), responses are
//! capped and requests time out; an unreachable agent is an error, never an empty inventory.

use std::time::Duration;

use async_trait::async_trait;
use atlas_api_types::{DiscoveryResult, MetricSample, StorageHealth, StoragePool, StorageVolume};
use atlas_driver_core::{DriverError, StorageDriver};

use crate::volumes_of;

const MAX_BYTES: usize = 8 * 1024 * 1024;

pub struct RemoteMacosDriver {
    backend_id: String,
    base_url: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl RemoteMacosDriver {
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
