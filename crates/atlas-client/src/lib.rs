// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Typed client for the Atlas REST API, for Zyvor products (Transiva, Veyron, GuestKit, ...).
//!
//! Products state intent (a name, a size, a policy, who owns it) and follow the job Atlas
//! returns; they never see pools, StorageClasses or backend quirks. Request and response types are
//! the gateway's own (`atlas-api-types`), so they cannot drift. Writes are asynchronous: they
//! return a job, and [`AtlasClient::wait_for_job`] follows it to a terminal state.
//!
//! Owner convention: see `docs/PRODUCTS.md`; the JWT subject for a product is
//! `product.service.<name>`.

use std::time::Duration;

use atlas_api_types::{job_state, CreateVolumeRequest, JobRecord};
use serde::Deserialize;
use serde_json::Value;

const API: &str = "/api/atlas/v1";

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("request failed: {0}")]
    Transport(#[from] reqwest::Error),
    /// The gateway answered with a non-success status; `body` is its error text.
    #[error("atlas returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("job {job_id} did not finish within {waited:?} (last state {state})")]
    Timeout {
        job_id: String,
        state: String,
        waited: Duration,
    },
    #[error("job {job_id} ended {state}: {error}")]
    JobFailed {
        job_id: String,
        state: String,
        error: String,
    },
}

/// What a write returns: the job to follow and the resource it creates or changes.
#[derive(Debug, Clone, Deserialize)]
pub struct Accepted {
    pub job_id: String,
    pub state: String,
    #[serde(default)]
    pub resource: Value,
}

#[derive(Clone)]
pub struct AtlasClient {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl AtlasClient {
    /// `base` is the gateway origin, e.g. `https://atlas.example:5110`. `token` is a bearer token
    /// (omit when the gateway runs without auth).
    pub fn new(base: impl Into<String>, token: Option<String>) -> Self {
        Self::with_http(base, token, reqwest::Client::new())
    }

    /// Use a caller-configured HTTP client (custom CA, timeouts, proxies).
    pub fn with_http(
        base: impl Into<String>,
        token: Option<String>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            token,
            http,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{API}{path}", self.base)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let r = self.http.request(method, self.url(path));
        match &self.token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    async fn send<T: for<'de> Deserialize<'de>>(
        req: reqwest::RequestBuilder,
    ) -> Result<T, ClientError> {
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ClientError::Status {
                status: status.as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }
        Ok(resp.json().await?)
    }

    /// `POST /volumes`. Returns the accepted job; follow it with [`Self::wait_for_job`].
    pub async fn create_volume(&self, req: &CreateVolumeRequest) -> Result<Accepted, ClientError> {
        Self::send(self.request(reqwest::Method::POST, "/volumes").json(req)).await
    }

    /// `GET /volumes`, optionally filtered by tenant. Returned as JSON because the listing carries
    /// backend-specific fields.
    pub async fn list_volumes(&self, tenant: Option<&str>) -> Result<Value, ClientError> {
        let mut r = self.request(reqwest::Method::GET, "/volumes");
        if let Some(t) = tenant {
            r = r.query(&[("tenant", t)]);
        }
        Self::send(r).await
    }

    /// `GET /jobs/{id}`.
    pub async fn get_job(&self, id: &str) -> Result<JobRecord, ClientError> {
        Self::send(self.request(reqwest::Method::GET, &format!("/jobs/{id}"))).await
    }

    /// Poll a job every `poll` until it succeeds, fails, is cancelled, or `timeout` passes.
    /// A failed or cancelled job is an error carrying the job's own message.
    pub async fn wait_for_job(
        &self,
        id: &str,
        poll: Duration,
        timeout: Duration,
    ) -> Result<JobRecord, ClientError> {
        let started = tokio::time::Instant::now();
        loop {
            let job = self.get_job(id).await?;
            match job.state.as_str() {
                job_state::SUCCEEDED => return Ok(job),
                job_state::FAILED | "cancelled" | "canceled" => {
                    return Err(ClientError::JobFailed {
                        job_id: job.id,
                        state: job.state,
                        error: job.error.unwrap_or_default(),
                    })
                }
                _ => {}
            }
            if started.elapsed() >= timeout {
                return Err(ClientError::Timeout {
                    job_id: job.id,
                    state: job.state,
                    waited: timeout,
                });
            }
            tokio::time::sleep(poll).await;
        }
    }
}
