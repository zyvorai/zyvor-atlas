// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Blocking client for a native cluster's `/v1/fs` API. Requests go to whichever metadata node
//! is the leader: "not the leader" (421), "try again" (503) and transport failures move on to
//! the next endpoint and repeat for up to [`ClientConfig::retry_for`].

use std::{
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

use reqwest::{blocking, Method, StatusCode};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Base URLs of the metadata nodes, e.g. `https://atlas-native-0:7400`.
    pub endpoints: Vec<String>,
    pub token: Option<String>,
    /// PEM CA bundle trusted for HTTPS endpoints.
    pub ca_pem: Option<Vec<u8>>,
    /// PEM client certificate followed by its private key, for nodes that require mTLS.
    pub identity_pem: Option<Vec<u8>>,
    /// How long to keep retrying through an election or an unreachable node.
    pub retry_for: Duration,
    pub request_timeout: Duration,
}

impl ClientConfig {
    pub fn new(endpoints: Vec<String>) -> Self {
        Self {
            endpoints,
            token: None,
            ca_pem: None,
            identity_pem: None,
            retry_for: Duration::from_secs(30),
            request_timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{status} {code}: {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
    },
    #[error("cluster unreachable: {0}")]
    Transport(String),
    #[error("unexpected response: {0}")]
    Decode(String),
    #[error("client setup: {0}")]
    Config(String),
}

impl Error {
    /// The errno a filesystem call should fail with.
    pub fn errno(&self) -> i32 {
        match self {
            Error::Api { code, status, .. } => match code.as_str() {
                "not_found" => libc::ENOENT,
                "exists" => libc::EEXIST,
                "not_empty" => libc::ENOTEMPTY,
                "not_dir" => libc::ENOTDIR,
                "is_dir" => libc::EISDIR,
                "read_only" => libc::EROFS,
                "invalid" => libc::EINVAL,
                "no_attr" => libc::ENODATA,
                "too_big" => libc::E2BIG,
                "unsupported" => libc::EOPNOTSUPP,
                "locked" => libc::EAGAIN,
                "quota" => libc::EDQUOT,
                "no_session" => libc::ENOLCK,
                _ if *status == 413 => libc::EFBIG,
                _ => libc::EIO,
            },
            _ => libc::EIO,
        }
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::Api { code, .. } if code == "not_found")
    }
}

pub enum Body<'a> {
    Empty,
    Json(Value),
    Bytes(&'a [u8]),
}

/// How a retried request may be judged.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Repeating it is harmless (reads, creates with a client id, absolute writes and setattrs).
    Idempotent,
    /// A removal: "not found" after an attempt whose outcome is unknown means it already happened.
    Remove,
}

enum Round {
    Done(Vec<u8>),
    Failed(Error),
    /// Every node said "not the leader"/"try again" or was unreachable.
    Again(Error),
}

pub struct Client {
    http: blocking::Client,
    cfg: ClientConfig,
    preferred: AtomicUsize,
}

impl Client {
    pub fn new(cfg: ClientConfig) -> Result<Self, Error> {
        if cfg.endpoints.is_empty() {
            return Err(Error::Config("at least one endpoint is required".into()));
        }
        let mut b = blocking::Client::builder()
            .timeout(cfg.request_timeout)
            .connect_timeout(Duration::from_secs(5))
            // Below the node's 30 s idle close, so a pooled connection is never reused as it closes.
            .pool_idle_timeout(Duration::from_secs(20));
        if let Some(pem) = &cfg.ca_pem {
            for c in reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|e| Error::Config(format!("CA bundle: {e}")))?
            {
                b = b.add_root_certificate(c);
            }
        }
        if let Some(pem) = &cfg.identity_pem {
            b = b.identity(
                reqwest::Identity::from_pem(pem)
                    .map_err(|e| Error::Config(format!("client identity: {e}")))?,
            );
        }
        let http = b.build().map_err(|e| Error::Config(e.to_string()))?;
        Ok(Self {
            http,
            cfg,
            preferred: AtomicUsize::new(0),
        })
    }

    pub fn request(
        &self,
        method: Method,
        path: &str,
        body: Body<'_>,
        retry: Retry,
    ) -> Result<Vec<u8>, Error> {
        let deadline = Instant::now() + self.cfg.retry_for;
        let mut ambiguous = false;
        let mut pause = Duration::from_millis(50);
        loop {
            match self.round(&method, path, &body, retry, &mut ambiguous) {
                Round::Done(b) => return Ok(b),
                Round::Failed(e) => return Err(e),
                Round::Again(e) if Instant::now() >= deadline => return Err(e),
                Round::Again(_) => {
                    thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_secs(1));
                }
            }
        }
    }

    pub fn json(
        &self,
        method: Method,
        path: &str,
        body: Body<'_>,
        retry: Retry,
    ) -> Result<Value, Error> {
        let b = self.request(method, path, body, retry)?;
        serde_json::from_slice(&b).map_err(|e| Error::Decode(format!("{path}: {e}")))
    }

    fn round(
        &self,
        method: &Method,
        path: &str,
        body: &Body<'_>,
        retry: Retry,
        ambiguous: &mut bool,
    ) -> Round {
        let n = self.cfg.endpoints.len();
        let start = self.preferred.load(Ordering::Relaxed) % n;
        let mut last = Error::Transport("no endpoint answered".into());
        for k in 0..n {
            let i = (start + k) % n;
            let url = format!("{}{path}", self.cfg.endpoints[i].trim_end_matches('/'));
            let mut req = self.http.request(method.clone(), &url);
            if let Some(t) = &self.cfg.token {
                req = req.bearer_auth(t);
            }
            req = match body {
                Body::Empty => req,
                Body::Json(v) => req.json(v),
                Body::Bytes(b) => req.body(b.to_vec()),
            };
            let resp = match req.send() {
                Ok(r) => r,
                Err(e) => {
                    // The request may have reached the leader before the connection failed.
                    *ambiguous = true;
                    last = Error::Transport(format!("{url}: {e}"));
                    continue;
                }
            };
            let status = resp.status();
            let bytes = match resp.bytes() {
                Ok(b) => b.to_vec(),
                Err(e) => {
                    *ambiguous = true;
                    last = Error::Transport(format!("{url}: {e}"));
                    continue;
                }
            };
            if status.is_success() {
                self.preferred.store(i, Ordering::Relaxed);
                return Round::Done(bytes);
            }
            let err = api_error(status, &bytes);
            match status {
                StatusCode::MISDIRECTED_REQUEST => last = err,
                StatusCode::SERVICE_UNAVAILABLE => {
                    // A proposal that timed out may still commit.
                    *ambiguous = true;
                    last = err;
                }
                StatusCode::NOT_FOUND if retry == Retry::Remove && *ambiguous => {
                    self.preferred.store(i, Ordering::Relaxed);
                    return Round::Done(Vec::new());
                }
                _ => {
                    self.preferred.store(i, Ordering::Relaxed);
                    return Round::Failed(err);
                }
            }
        }
        Round::Again(last)
    }
}

fn api_error(status: StatusCode, body: &[u8]) -> Error {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    Error::Api {
        status: status.as_u16(),
        code: v["code"].as_str().unwrap_or_default().to_string(),
        message: v["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned()),
    }
}

/// Percent-encodes a query value (RFC 3986 unreserved characters pass through).
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_codes_to_errno() {
        let e = |code: &str, status| Error::Api {
            status,
            code: code.into(),
            message: String::new(),
        };
        assert_eq!(e("not_found", 404).errno(), libc::ENOENT);
        assert_eq!(e("not_empty", 409).errno(), libc::ENOTEMPTY);
        assert_eq!(e("read_only", 409).errno(), libc::EROFS);
        assert_eq!(e("", 413).errno(), libc::EFBIG);
        assert_eq!(e("internal", 500).errno(), libc::EIO);
        assert_eq!(Error::Transport("x".into()).errno(), libc::EIO);
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode("a b/é+"), "a%20b%2F%C3%A9%2B");
        assert_eq!(encode("plain-name_1.txt"), "plain-name_1.txt");
    }
}
