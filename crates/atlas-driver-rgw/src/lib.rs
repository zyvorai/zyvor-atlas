// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Minimal, generic S3 client (PDF §10.3 "Atlas → RGW: S3 API") — shared by Ceph RGW and any other
//! S3-compatible backend (Ceph RGW, MinIO, Garage, ...). Nothing here is Ceph-specific: it's plain SigV4 signing
//! against whatever endpoint/bucket/credentials the caller supplies.
//!
//! Uses `rusty-s3` to build SigV4-signed request URLs and `reqwest` to execute them — no heavy
//! AWS SDK. Path-style addressing (required by RGW, and the conventional choice for other
//! S3-compatible servers too). Credentials are passed in by the caller (read from a Kubernetes
//! Secret in-cluster); this crate never persists or logs them.

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use rusty_s3::actions::{CreateMultipartUpload, ListObjectsV2};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const SIGN_TTL: Duration = Duration::from_secs(300);

/// S3 minimum part size (5 MiB). Every multipart part except the last must be at least this.
pub const MIN_PART_SIZE: usize = 5 * 1024 * 1024;

/// A handle to one bucket on an S3/RGW endpoint.
pub struct S3Target {
    bucket: Bucket,
    creds: Credentials,
    http: reqwest::Client,
}

impl S3Target {
    /// Build from an endpoint URL (`http://host:port`), region, bucket name, and credentials.
    pub fn new(
        endpoint: &str,
        region: &str,
        bucket_name: &str,
        access_key: &str,
        secret_key: &str,
    ) -> Result<Self> {
        let url: url::Url = endpoint.parse().context("invalid S3 endpoint URL")?;
        let region = if region.is_empty() {
            "us-east-1"
        } else {
            region
        };
        let bucket = Bucket::new(
            url,
            UrlStyle::Path,
            bucket_name.to_string(),
            region.to_string(),
        )
        .context("invalid bucket")?;
        Ok(Self {
            bucket,
            creds: Credentials::new(access_key, secret_key),
            // No timeout: matches the previous `reqwest::Client::new()` — multipart uploads/downloads of
            // large objects must not be capped by a fixed per-request deadline.
            http: atlas_driver_core::trusted_http_client(None)?,
        })
    }

    /// PUT an object.
    pub async fn put_object(&self, key: &str, body: Vec<u8>) -> Result<()> {
        let action = self.bucket.put_object(Some(&self.creds), key);
        let signed = action.sign(SIGN_TTL);
        let resp = self
            .http
            .put(signed)
            .body(body)
            .send()
            .await
            .with_context(|| format!("PUT {key}"))?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            anyhow::bail!("PUT {key} failed: HTTP {status}: {detail}");
        }
        Ok(())
    }

    /// GET an object's bytes.
    pub async fn get_object(&self, key: &str) -> Result<Vec<u8>> {
        let action = self.bucket.get_object(Some(&self.creds), key);
        let signed = action.sign(SIGN_TTL);
        let resp = self
            .http
            .get(signed)
            .send()
            .await
            .with_context(|| format!("GET {key}"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("GET {key} failed: HTTP {status}");
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// Whether an object exists (via GET; RGW/reqwest keep this simple for the MVP).
    pub async fn object_exists(&self, key: &str) -> bool {
        self.get_object(key).await.is_ok()
    }

    /// A time-limited presigned GET URL for `key` (client can download without credentials).
    pub fn presigned_get(&self, key: &str, ttl_secs: u64) -> String {
        self.bucket
            .get_object(Some(&self.creds), key)
            .sign(Duration::from_secs(ttl_secs))
            .to_string()
    }

    /// A time-limited presigned PUT URL for `key` (client can upload directly to RGW without
    /// credentials — the gateway never sees the object bytes). SigV4 query auth, so the browser
    /// just does `fetch(url, { method: "PUT", body: file })`.
    pub fn presigned_put(&self, key: &str, ttl_secs: u64) -> String {
        self.bucket
            .put_object(Some(&self.creds), key)
            .sign(Duration::from_secs(ttl_secs))
            .to_string()
    }

    /// Stream `src` to `key` as an S3 multipart upload, hashing the bytes as they pass through.
    /// Returns `(total_bytes, sha256_hex)`. Never buffers the whole object — only one `part_size`
    /// chunk at a time (clamped to the S3 5 MiB minimum). Aborts the upload on any error.
    pub async fn put_multipart_streaming<R>(
        &self,
        key: &str,
        part_size: usize,
        mut src: R,
    ) -> Result<(u64, String)>
    where
        R: AsyncRead + Unpin,
    {
        let part_size = part_size.max(MIN_PART_SIZE);
        let action = self.bucket.create_multipart_upload(Some(&self.creds), key);
        let resp = self
            .http
            .post(action.sign(SIGN_TTL))
            .send()
            .await
            .with_context(|| format!("initiate multipart {key}"))?;
        if !resp.status().is_success() {
            let s = resp.status();
            let d = resp.text().await.unwrap_or_default();
            anyhow::bail!("initiate multipart {key} failed: HTTP {s}: {d}");
        }
        let body = resp.text().await?;
        let upload_id = CreateMultipartUpload::parse_response(&body)
            .context("parse multipart initiate response")?
            .upload_id()
            .to_string();

        match self
            .upload_all_parts(key, &upload_id, part_size, &mut src)
            .await
        {
            Ok((total, sha, etags)) => {
                let action = self.bucket.complete_multipart_upload(
                    Some(&self.creds),
                    key,
                    &upload_id,
                    etags.iter().map(String::as_str),
                );
                let signed = action.sign(SIGN_TTL);
                let xml = action.body();
                let resp = self
                    .http
                    .post(signed)
                    .body(xml)
                    .send()
                    .await
                    .with_context(|| format!("complete multipart {key}"))?;
                if !resp.status().is_success() {
                    let s = resp.status();
                    let d = resp.text().await.unwrap_or_default();
                    let _ = self.abort_multipart(key, &upload_id).await;
                    anyhow::bail!("complete multipart {key} failed: HTTP {s}: {d}");
                }
                // S3 may answer 200 and report a failure in the body, which arrives only once
                // the object is assembled.
                let d = resp
                    .text()
                    .await
                    .with_context(|| format!("complete multipart {key}"))?;
                if d.contains("<Error>") {
                    let _ = self.abort_multipart(key, &upload_id).await;
                    anyhow::bail!("complete multipart {key} failed: {d}");
                }
                Ok((total, sha))
            }
            Err(e) => {
                let _ = self.abort_multipart(key, &upload_id).await;
                Err(e)
            }
        }
    }

    async fn upload_all_parts<R>(
        &self,
        key: &str,
        upload_id: &str,
        part_size: usize,
        src: &mut R,
    ) -> Result<(u64, String, Vec<String>)>
    where
        R: AsyncRead + Unpin,
    {
        let mut hasher = Sha256::new();
        let mut etags: Vec<String> = Vec::new();
        let mut total: u64 = 0;
        let mut part_number: u16 = 1;
        let mut buf = vec![0u8; part_size];
        loop {
            // Fill a full part before uploading (a short read only means "not yet EOF").
            let mut filled = 0;
            while filled < part_size {
                let n = src
                    .read(&mut buf[filled..])
                    .await
                    .context("read part from source")?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            hasher.update(&buf[..filled]);
            total += filled as u64;
            let etag = self
                .upload_one_part(key, upload_id, part_number, buf[..filled].to_vec())
                .await?;
            etags.push(etag);
            part_number += 1;
            if filled < part_size {
                break; // reached EOF on this part
            }
        }
        // S3 requires at least one part; upload an empty final part for an empty source.
        if etags.is_empty() {
            let etag = self.upload_one_part(key, upload_id, 1, Vec::new()).await?;
            etags.push(etag);
        }
        Ok((total, hex::encode(hasher.finalize()), etags))
    }

    async fn upload_one_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u16,
        body: Vec<u8>,
    ) -> Result<String> {
        let action = self
            .bucket
            .upload_part(Some(&self.creds), key, part_number, upload_id);
        let resp = self
            .http
            .put(action.sign(SIGN_TTL))
            .body(body)
            .send()
            .await
            .with_context(|| format!("upload part {part_number} of {key}"))?;
        if !resp.status().is_success() {
            let s = resp.status();
            let d = resp.text().await.unwrap_or_default();
            anyhow::bail!("upload part {part_number} of {key} failed: HTTP {s}: {d}");
        }
        resp.headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("upload part {part_number} of {key}: missing ETag"))
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<()> {
        let action = self
            .bucket
            .abort_multipart_upload(Some(&self.creds), key, upload_id);
        let _ = self.http.delete(action.sign(SIGN_TTL)).send().await;
        Ok(())
    }

    /// Stream `key` from S3 into `sink`, hashing the bytes as they pass through. Returns
    /// `(total_bytes, sha256_hex)`. Never buffers the whole object in memory.
    pub async fn get_object_streaming<W>(&self, key: &str, mut sink: W) -> Result<(u64, String)>
    where
        W: AsyncWrite + Unpin,
    {
        let action = self.bucket.get_object(Some(&self.creds), key);
        let resp = self
            .http
            .get(action.sign(SIGN_TTL))
            .send()
            .await
            .with_context(|| format!("GET {key}"))?;
        if !resp.status().is_success() {
            anyhow::bail!("GET {key} failed: HTTP {}", resp.status());
        }
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("stream body {key}"))?;
            hasher.update(&chunk);
            total += chunk.len() as u64;
            sink.write_all(&chunk)
                .await
                .context("write streamed object to sink")?;
        }
        sink.flush().await.ok();
        Ok((total, hex::encode(hasher.finalize())))
    }

    /// List objects in the bucket (optionally under `prefix`) as `(key, size_bytes)` pairs.
    /// Follows `next_continuation_token` so buckets/prefixes with more than one page (>1000
    /// objects by default) are returned in full rather than silently truncated at page one.
    pub async fn list_objects(&self, prefix: Option<&str>) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        let mut continuation_token: Option<String> = None;
        loop {
            let mut action = self.bucket.list_objects_v2(Some(&self.creds));
            if let Some(p) = prefix {
                action.query_mut().insert("prefix", p.to_owned());
            }
            if let Some(ref tok) = continuation_token {
                action.with_continuation_token(tok.clone());
            }
            let resp = self
                .http
                .get(action.sign(SIGN_TTL))
                .send()
                .await
                .context("list objects")?;
            if !resp.status().is_success() {
                anyhow::bail!("list objects failed: HTTP {}", resp.status());
            }
            let body = resp.text().await?;
            let parsed = ListObjectsV2::parse_response(&body).context("parse list objects")?;
            // ListObjectsV2 is requested with encoding-type=url, so RGW/S3 return keys
            // percent-encoded (e.g. "models/x" -> "models%2Fx"). Decode them so callers get the
            // real key — otherwise any prefixed key 404s on the subsequent GET/PUT.
            out.extend(parsed.contents.into_iter().map(|c| {
                let key = percent_encoding::percent_decode_str(&c.key)
                    .decode_utf8_lossy()
                    .into_owned();
                (key, c.size.unwrap_or(0))
            }));
            match parsed.next_continuation_token {
                Some(tok) => continuation_token = Some(tok),
                None => break,
            }
        }
        Ok(out)
    }

    /// DELETE an object. S3 delete is idempotent (deleting a missing key returns success).
    pub async fn delete_object(&self, key: &str) -> Result<()> {
        let action = self.bucket.delete_object(Some(&self.creds), key);
        let signed = action.sign(SIGN_TTL);
        let resp = self
            .http
            .delete(signed)
            .send()
            .await
            .with_context(|| format!("DELETE {key}"))?;
        let status = resp.status();
        // 204/200 on success; 404 is fine (already gone).
        if !status.is_success() && status.as_u16() != 404 {
            let detail = resp.text().await.unwrap_or_default();
            anyhow::bail!("DELETE {key} failed: HTTP {status}: {detail}");
        }
        Ok(())
    }

    /// Whether the bucket itself already exists and is reachable with these credentials
    /// (S3 `HeadBucket`). The presigned URL is signed for HEAD, so it must be sent as HEAD — a GET
    /// fails signature verification and would always read as "missing".
    pub async fn bucket_exists(&self) -> bool {
        let action = self.bucket.head_bucket(Some(&self.creds));
        matches!(
            self.http.head(action.sign(SIGN_TTL)).send().await,
            Ok(r) if r.status().is_success()
        )
    }

    /// Create the bucket itself (S3 `CreateBucket`). For any region other than `us-east-1` the
    /// request carries a `CreateBucketConfiguration` body naming it as `LocationConstraint` — real
    /// AWS S3 rejects a non-default-region create without it. Not idempotent by itself — callers
    /// that want create-if-missing semantics should check `bucket_exists` first.
    pub async fn create_bucket(&self) -> Result<()> {
        self.create_bucket_with_object_lock(false).await
    }

    /// Create the bucket with S3 Object Lock (WORM retention) enabled — `x-amz-bucket-object-lock-enabled`,
    /// an S3-standard header set only at creation; S3 stores refuse to enable it retroactively on
    /// an existing bucket. Enabling it also turns bucket versioning on server-side (S3 requires
    /// versioning for object lock). Like the region body below, this header rides on a presigned PUT
    /// outside the signed query string — a live S3 server accepted the analogous unsigned region body, so
    /// the same leniency is expected here; verify live rather than trusting this comment.
    pub async fn create_bucket_with_object_lock(&self, enable_object_lock: bool) -> Result<()> {
        let mut action = self.bucket.create_bucket(&self.creds);
        // Any extra header on a presigned request must be part of X-Amz-SignedHeaders or the
        // server rejects it ("headers present which were not signed") — found live. `headers_mut`
        // adds it to the signature; the identical header must then be sent on the actual request.
        if enable_object_lock {
            action
                .headers_mut()
                .insert("x-amz-bucket-object-lock-enabled", "true");
        }
        let mut req = self.http.put(action.sign(SIGN_TTL));
        if let Some(body) = create_bucket_body(self.bucket.region()) {
            req = req.header("content-type", "application/xml").body(body);
        }
        if enable_object_lock {
            req = req.header("x-amz-bucket-object-lock-enabled", "true");
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("CreateBucket {}", self.bucket.name()))?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "CreateBucket {} failed: HTTP {status}: {detail}",
                self.bucket.name()
            );
        }
        Ok(())
    }

    /// Delete the bucket itself (S3 `DeleteBucket`). The bucket must be empty; a non-empty bucket
    /// returns an error (S3's own `409 BucketNotEmpty`), same as every other S3-compatible server.
    /// 404 is treated as success, matching `delete_object`'s idempotency convention.
    pub async fn delete_bucket(&self) -> Result<()> {
        let action = self.bucket.delete_bucket(&self.creds);
        let resp = self
            .http
            .delete(action.sign(SIGN_TTL))
            .send()
            .await
            .with_context(|| format!("DeleteBucket {}", self.bucket.name()))?;
        let status = resp.status();
        if !status.is_success() && status.as_u16() != 404 {
            let detail = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "DeleteBucket {} failed: HTTP {status}: {detail}",
                self.bucket.name()
            );
        }
        Ok(())
    }

    /// GET a bucket-level S3 subresource (`?versioning`, `?lifecycle`, `?policy`, `?tagging`,
    /// `?cors`, `?object-lock`, `?encryption`, ...) — the console's per-bucket Settings panel.
    /// rusty-s3 has no dedicated action type for most of these (only `create_bucket`/
    /// `delete_bucket` are covered), so this signs the request directly rather than adding one
    /// bespoke `S3Action` impl per subresource. Returns the raw status + body (XML/JSON, backend-
    /// dependent) unparsed — same "pass the server's own response through" approach the removed
    /// backend-agnostic S3 proxy.
    pub async fn get_bucket_subresource(&self, subresource: &str) -> Result<(u16, String)> {
        let resp = self
            .http
            .get(self.sign_subresource(rusty_s3::Method::Get, subresource))
            .send()
            .await
            .with_context(|| format!("GET ?{subresource} on {}", self.bucket.name()))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok((status, body))
    }

    /// PUT a bucket-level S3 subresource — see `get_bucket_subresource`.
    pub async fn put_bucket_subresource(
        &self,
        subresource: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<(u16, String)> {
        let resp = self
            .http
            .put(self.sign_subresource(rusty_s3::Method::Put, subresource))
            .header("content-type", content_type)
            .body(body)
            .send()
            .await
            .with_context(|| format!("PUT ?{subresource} on {}", self.bucket.name()))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok((status, body))
    }

    /// DELETE a bucket-level S3 subresource — see `get_bucket_subresource`.
    pub async fn delete_bucket_subresource(&self, subresource: &str) -> Result<(u16, String)> {
        let resp = self
            .http
            .delete(self.sign_subresource(rusty_s3::Method::Delete, subresource))
            .send()
            .await
            .with_context(|| format!("DELETE ?{subresource} on {}", self.bucket.name()))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok((status, body))
    }

    /// Sign a bare `?<subresource>` request (no other query params, matching every subresource
    /// this proxy forwards) using rusty-s3's public `signing::sign` directly.
    fn sign_subresource(&self, method: rusty_s3::Method, subresource: &str) -> url::Url {
        rusty_s3::signing::sign(
            &jiff::Timestamp::now(),
            method,
            self.bucket.base_url().clone(),
            self.creds.key(),
            self.creds.secret(),
            self.creds.token(),
            self.bucket.region(),
            SIGN_TTL.as_secs(),
            std::iter::once((subresource, "")),
            std::iter::empty(),
        )
    }
}

/// The `CreateBucket` request body for `region`: none for the default `us-east-1`, otherwise a
/// `CreateBucketConfiguration` with the region as `LocationConstraint`.
fn create_bucket_body(region: &str) -> Option<String> {
    if region.is_empty() || region == "us-east-1" {
        return None;
    }
    Some(format!(
        "<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <LocationConstraint>{region}</LocationConstraint></CreateBucketConfiguration>"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_bucket_body_only_for_non_default_regions() {
        assert!(create_bucket_body("us-east-1").is_none());
        assert!(create_bucket_body("").is_none());
        let body = create_bucket_body("eu-west-1").unwrap();
        assert!(body.contains("<LocationConstraint>eu-west-1</LocationConstraint>"));
    }

    /// Regression: an object-lock CreateBucket sends `x-amz-bucket-object-lock-enabled`. The S3 server
    /// rejects any header that rides along unsigned ("headers present which were not signed",
    /// found live) — the header must be part of the presigned URL's X-Amz-SignedHeaders.
    #[test]
    fn object_lock_header_is_part_of_the_signed_headers() {
        let creds = rusty_s3::Credentials::new("ak", "sk");
        let bucket = rusty_s3::Bucket::new(
            url::Url::parse("http://h:9000").unwrap(),
            UrlStyle::Path,
            "b",
            "us-east-1",
        )
        .unwrap();
        let mut action = bucket.create_bucket(&creds);
        action
            .headers_mut()
            .insert("x-amz-bucket-object-lock-enabled", "true");
        let url = action.sign(SIGN_TTL);
        let signed_headers = url
            .query_pairs()
            .find(|(k, _)| k == "X-Amz-SignedHeaders")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        assert!(signed_headers.contains("x-amz-bucket-object-lock-enabled"));
    }

    /// A port nothing is listening on — connection refused immediately, no real network I/O.
    /// Same "never fabricates success" convention used elsewhere in this codebase (e.g.
    /// `RealZfsDriver`'s "missing binary errors instead of fabricating data" tests): these prove
    /// the new bucket-lifecycle methods are wired to the right HTTP verb and don't panic, without
    /// needing a live S3-compatible server.
    fn unreachable_target() -> S3Target {
        S3Target::new("http://127.0.0.1:1", "us-east-1", "test-bucket", "ak", "sk").unwrap()
    }

    #[tokio::test]
    async fn create_bucket_against_unreachable_endpoint_errors_not_panics() {
        let t = unreachable_target();
        assert!(t.create_bucket().await.is_err());
    }

    #[tokio::test]
    async fn delete_bucket_against_unreachable_endpoint_errors_not_panics() {
        let t = unreachable_target();
        assert!(t.delete_bucket().await.is_err());
    }

    #[tokio::test]
    async fn bucket_exists_against_unreachable_endpoint_is_false_not_a_panic() {
        let t = unreachable_target();
        assert!(!t.bucket_exists().await);
    }

    #[test]
    fn create_bucket_signs_a_path_style_put_to_the_bucket_root() {
        let t = unreachable_target();
        let url = t.bucket.create_bucket(&t.creds).sign(SIGN_TTL);
        // Path-style addressing: bucket name is a path segment, not a subdomain.
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.path(), "/test-bucket/");
        assert!(url.query().unwrap_or_default().contains("X-Amz-Signature="));
    }

    #[test]
    fn delete_bucket_signs_a_path_style_delete_to_the_bucket_root() {
        let t = unreachable_target();
        let url = t.bucket.delete_bucket(&t.creds).sign(SIGN_TTL);
        assert_eq!(url.path(), "/test-bucket/");
        assert!(url.query().unwrap_or_default().contains("X-Amz-Signature="));
    }
}
