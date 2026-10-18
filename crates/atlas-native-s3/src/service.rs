// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0

//! The S3 API over [`Bucket`]s, served by s3s. Store calls block, so each runs on Tokio's blocking
//! pool; object bodies stream through in [`CHUNK`]-sized pieces.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures_util::StreamExt;
use s3s::{
    auth::Credentials, checksum::ChecksumHasher, dto::*, s3_error, S3Error, S3ErrorCode, S3Request,
    S3Response, S3Result, S3,
};

use crate::{
    checksum,
    store::{hex, Bucket, Headers, Listed, StoreError, Writer, CHUNK, EMPTY_ETAG},
};

/// What one access key may do.
#[derive(Debug, Clone, Default)]
pub struct Grant {
    /// `None`: every bucket.
    pub buckets: Option<BTreeSet<String>>,
    pub read_only: bool,
}

/// Cheap to clone: clones share the buckets.
#[derive(Clone)]
pub struct Gateway {
    buckets: BTreeMap<String, Arc<Bucket>>,
    /// Access key → grant. `None` serves every request without authentication (tests only; the
    /// binary always configures credentials).
    grants: Option<HashMap<String, Grant>>,
}

impl Gateway {
    pub fn new(buckets: Vec<Bucket>, grants: Option<HashMap<String, Grant>>) -> Self {
        Self {
            buckets: buckets
                .into_iter()
                .map(|b| (b.name.clone(), Arc::new(b)))
                .collect(),
            grants,
        }
    }

    pub fn buckets(&self) -> impl Iterator<Item = &Arc<Bucket>> {
        self.buckets.values()
    }

    fn grant(&self, creds: Option<&Credentials>) -> S3Result<Option<&Grant>> {
        let Some(grants) = &self.grants else {
            return Ok(None);
        };
        let key = creds
            .map(|c| c.access_key.as_str())
            .ok_or_else(|| s3_error!(AccessDenied))?;
        grants
            .get(key)
            .map(Some)
            .ok_or_else(|| s3_error!(AccessDenied))
    }

    /// The bucket, if the caller may read it (or, with `write`, change it).
    fn bucket(
        &self,
        creds: Option<&Credentials>,
        name: &str,
        write: bool,
    ) -> S3Result<Arc<Bucket>> {
        if let Some(g) = self.grant(creds)? {
            if g.buckets.as_ref().is_some_and(|b| !b.contains(name)) || (write && g.read_only) {
                return Err(s3_error!(AccessDenied));
            }
        }
        let b = self
            .buckets
            .get(name)
            .ok_or_else(|| s3_error!(NoSuchBucket))?;
        if write && b.is_read_only() {
            return Err(s3_error!(AccessDenied, "the bucket is read-only"));
        }
        Ok(b.clone())
    }
}

fn s3_err(e: StoreError) -> S3Error {
    let custom = |code: &'static str, status: u16, msg: String| {
        let mut err = S3Error::with_message(S3ErrorCode::Custom(code.into()), msg);
        if let Ok(s) = http::StatusCode::from_u16(status) {
            err.set_status_code(s);
        }
        err
    };
    match e {
        StoreError::NoSuchKey => s3_error!(NoSuchKey),
        StoreError::NoSuchUpload => s3_error!(NoSuchUpload),
        StoreError::InvalidKey(m) | StoreError::Invalid(m) => {
            S3Error::with_message(S3ErrorCode::InvalidArgument, m)
        }
        StoreError::Conflict(m) => custom("ObjectConflict", 409, m),
        StoreError::ReadOnly => s3_error!(AccessDenied, "the bucket is read-only"),
        StoreError::Locked(m) => S3Error::with_message(S3ErrorCode::AccessDenied, m),
        StoreError::Quota => custom("QuotaExceeded", 507, e.to_string()),
        StoreError::InvalidPart(m) => S3Error::with_message(S3ErrorCode::InvalidPart, m),
        StoreError::TooSmall(_) => {
            S3Error::with_message(S3ErrorCode::EntityTooSmall, e.to_string())
        }
        StoreError::BadDigest => s3_error!(BadDigest),
        StoreError::Io(_) => S3Error::with_message(S3ErrorCode::InternalError, e.to_string()),
    }
}

async fn blocking<T, F>(f: F) -> S3Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, StoreError> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| s3_error!(InternalError, "a storage task failed"))?
        .map_err(s3_err)
}

fn timestamp(ns: i64) -> Timestamp {
    Timestamp::from(UNIX_EPOCH + Duration::from_nanos(ns.max(0) as u64))
}

fn etag(v: String) -> ETag {
    ETag::Strong(v)
}

fn headers(
    content_type: Option<String>,
    content_encoding: Option<String>,
    content_disposition: Option<String>,
    content_language: Option<String>,
    cache_control: Option<String>,
    metadata: Option<Metadata>,
) -> Headers {
    Headers {
        content_type,
        content_encoding,
        content_disposition,
        content_language,
        cache_control,
        metadata: metadata.unwrap_or_default().into_iter().collect(),
    }
}

fn metadata(h: &Headers) -> Option<Metadata> {
    (!h.metadata.is_empty()).then(|| h.metadata.clone().into_iter().collect())
}

/// Decodes a base64 `Content-MD5`.
fn content_md5(v: Option<&str>) -> S3Result<Option<[u8; 16]>> {
    let Some(v) = v else { return Ok(None) };
    let bytes = base64_decode(v).ok_or_else(|| s3_error!(InvalidDigest))?;
    bytes
        .try_into()
        .map(Some)
        .map_err(|_| s3_error!(InvalidDigest))
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let s = s.trim().trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        let bytes = n.to_be_bytes();
        match chunk.len() {
            4 => out.extend_from_slice(&bytes[1..4]),
            3 => out.extend_from_slice(&bytes[1..3]),
            2 => out.push(bytes[1]),
            _ => return None,
        }
    }
    Some(out)
}

/// Percent-encodes a key for `encoding-type=url` listings (`/` and unreserved bytes pass).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Continuation tokens are the hex of the last key returned: opaque and XML-safe.
fn decode_token(t: &str) -> S3Result<String> {
    let bad = || s3_error!(InvalidArgument, "the continuation token is not valid");
    if !t.len().is_multiple_of(2) {
        return Err(bad());
    }
    let bytes = (0..t.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(t.get(i..i + 2).unwrap_or("zz"), 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|_| bad())?;
    String::from_utf8(bytes).map_err(|_| bad())
}

/// Streams a request body into `w`, hashing it with `hasher`; removes the file on failure.
async fn write_body(
    b: &Arc<Bucket>,
    mut w: Writer,
    mut hasher: ChecksumHasher,
    body: Option<StreamingBlob>,
) -> S3Result<(Writer, ChecksumHasher)> {
    let Some(mut body) = body else {
        return Ok((w, hasher));
    };
    let mut buf: Vec<u8> = Vec::with_capacity(CHUNK);
    loop {
        let done = match body.next().await {
            None => true,
            Some(Ok(bytes)) => {
                buf.extend_from_slice(&bytes);
                false
            }
            Some(Err(e)) => {
                tracing::debug!(error = %e, "request body failed");
                discard(b, w).await;
                return Err(s3_error!(IncompleteBody));
            }
        };
        if buf.len() >= CHUNK || (done && !buf.is_empty()) {
            let data = std::mem::replace(&mut buf, Vec::with_capacity(CHUNK));
            let bucket = b.clone();
            let joined = tokio::task::spawn_blocking(move || {
                hasher.update(&data);
                let r = bucket.write(&mut w, &data);
                (w, hasher, r)
            })
            .await;
            match joined {
                Ok((w2, h2, Ok(()))) => {
                    w = w2;
                    hasher = h2;
                }
                Ok((w2, _, Err(e))) => {
                    discard(b, w2).await;
                    return Err(s3_err(e));
                }
                Err(_) => return Err(s3_error!(InternalError, "a storage task failed")),
            }
        }
        if done {
            return Ok((w, hasher));
        }
    }
}

async fn discard(b: &Arc<Bucket>, w: Writer) {
    let b = b.clone();
    let _ = tokio::task::spawn_blocking(move || b.discard(w)).await;
}

/// Streams `len` bytes of file `ino` from `start`, reading ahead one chunk.
fn read_stream(b: Arc<Bucket>, ino: u64, start: u64, len: u64) -> StreamingBlob {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(2);
    tokio::spawn(async move {
        let (mut pos, end) = (start, start + len);
        while pos < end {
            let n = (end - pos).min(CHUNK as u64) as usize;
            let bucket = b.clone();
            let item = match tokio::task::spawn_blocking(move || bucket.read(ino, pos, n)).await {
                Ok(Ok(d)) if !d.is_empty() => {
                    pos += d.len() as u64;
                    Ok(Bytes::from(d))
                }
                Ok(Ok(_)) => Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the object shrank while it was read",
                )),
                Ok(Err(e)) => Err(std::io::Error::other(e.to_string())),
                Err(_) => Err(std::io::Error::other("a storage task failed")),
            };
            let failed = item.is_err();
            if tx.send(item).await.is_err() || failed {
                break;
            }
        }
    });
    StreamingBlob::wrap(futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)))
}

/// `bytes=first-last` (inclusive) of a `size`-byte object, as `(offset, len)`.
fn copy_range(range: Option<&str>, size: u64) -> S3Result<(u64, u64)> {
    let Some(r) = range else { return Ok((0, size)) };
    let bad = || s3_error!(InvalidRange);
    let (first, last) = r
        .strip_prefix("bytes=")
        .and_then(|r| r.split_once('-'))
        .ok_or_else(bad)?;
    let first: u64 = first.parse().map_err(|_| bad())?;
    let last: u64 = last.parse().map_err(|_| bad())?;
    if first > last || last >= size {
        return Err(bad());
    }
    Ok((first, last - first + 1))
}

/// GET and HEAD preconditions (RFC 7232 order; times compare at whole seconds, like
/// `Last-Modified`).
fn preconditions(
    etag: &str,
    mtime_ns: i64,
    if_match: Option<&ETagCondition>,
    if_none_match: Option<&ETagCondition>,
    if_modified_since: Option<&Timestamp>,
    if_unmodified_since: Option<&Timestamp>,
) -> S3Result<()> {
    let matches = |c: &ETagCondition| c.is_any() || c.as_etag().is_some_and(|e| e.value() == etag);
    let secs = mtime_ns.div_euclid(1_000_000_000);
    let since = |t: &Timestamp| time::OffsetDateTime::from(t.clone()).unix_timestamp();
    match if_match {
        Some(c) if !matches(c) => return Err(s3_error!(PreconditionFailed)),
        Some(_) => {}
        None => {
            if if_unmodified_since.is_some_and(|t| secs > since(t)) {
                return Err(s3_error!(PreconditionFailed));
            }
        }
    }
    match if_none_match {
        Some(c) if matches(c) => Err(s3_error!(NotModified)),
        Some(_) => Ok(()),
        None if if_modified_since.is_some_and(|t| secs <= since(t)) => Err(s3_error!(NotModified)),
        None => Ok(()),
    }
}

/// `x-amz-copy-source-if-*`: every failed condition is 412 for a copy.
async fn copy_preconditions(
    src: &Arc<Bucket>,
    key: &str,
    if_match: Option<ETagCondition>,
    if_none_match: Option<ETagCondition>,
    if_modified_since: Option<Timestamp>,
    if_unmodified_since: Option<Timestamp>,
) -> S3Result<()> {
    if if_match.is_none()
        && if_none_match.is_none()
        && if_modified_since.is_none()
        && if_unmodified_since.is_none()
    {
        return Ok(());
    }
    let (b, k) = (src.clone(), key.to_string());
    let o = blocking(move || b.head(&k)).await?;
    preconditions(
        &o.etag,
        o.mtime_ns,
        if_match.as_ref(),
        if_none_match.as_ref(),
        if_modified_since.as_ref(),
        if_unmodified_since.as_ref(),
    )
    .map_err(|e| {
        if *e.code() == S3ErrorCode::NotModified {
            s3_error!(PreconditionFailed)
        } else {
            e
        }
    })
}

fn source(cs: &CopySource) -> S3Result<(String, String)> {
    match cs {
        CopySource::Bucket { bucket, key, .. } => Ok((bucket.to_string(), key.to_string())),
        _ => Err(s3_error!(
            NotImplemented,
            "only bucket copy sources are supported"
        )),
    }
}

/// A directory marker (a key ending in `/`) holds no data.
async fn require_empty(body: Option<StreamingBlob>) -> S3Result<()> {
    if let Some(mut body) = body {
        while let Some(chunk) = body.next().await {
            if !chunk.map_err(|_| s3_error!(IncompleteBody))?.is_empty() {
                return Err(s3_error!(
                    InvalidArgument,
                    "a key ending in `/` cannot have a body"
                ));
            }
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl S3 for Gateway {
    async fn list_buckets(
        &self,
        req: S3Request<ListBucketsInput>,
    ) -> S3Result<S3Response<ListBucketsOutput>> {
        let grant = self.grant(req.credentials.as_ref())?.cloned();
        let visible: Vec<Arc<Bucket>> = self
            .buckets
            .values()
            .filter(|b| {
                grant
                    .as_ref()
                    .is_none_or(|g| g.buckets.as_ref().is_none_or(|s| s.contains(&b.name)))
            })
            .cloned()
            .collect();
        let listed = blocking(move || {
            Ok(visible
                .iter()
                .map(|b| (b.name.clone(), b.created_ns().unwrap_or(0)))
                .collect::<Vec<_>>())
        })
        .await?;
        Ok(S3Response::new(ListBucketsOutput {
            buckets: Some(
                listed
                    .into_iter()
                    .map(|(name, ns)| s3s::dto::Bucket {
                        name: Some(name),
                        creation_date: Some(timestamp(ns)),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }))
    }

    async fn head_bucket(
        &self,
        req: S3Request<HeadBucketInput>,
    ) -> S3Result<S3Response<HeadBucketOutput>> {
        self.bucket(req.credentials.as_ref(), &req.input.bucket, false)?;
        Ok(S3Response::new(HeadBucketOutput::default()))
    }

    async fn get_bucket_location(
        &self,
        req: S3Request<GetBucketLocationInput>,
    ) -> S3Result<S3Response<GetBucketLocationOutput>> {
        self.bucket(req.credentials.as_ref(), &req.input.bucket, false)?;
        Ok(S3Response::new(GetBucketLocationOutput::default()))
    }

    async fn create_bucket(
        &self,
        req: S3Request<CreateBucketInput>,
    ) -> S3Result<S3Response<CreateBucketOutput>> {
        if self
            .bucket(req.credentials.as_ref(), &req.input.bucket, false)
            .is_ok()
        {
            return Err(s3_error!(BucketAlreadyOwnedByYou));
        }
        Err(s3_error!(
            NotImplemented,
            "buckets are atlas-native filesystems; add one to the gateway's bucket list"
        ))
    }

    async fn delete_bucket(
        &self,
        req: S3Request<DeleteBucketInput>,
    ) -> S3Result<S3Response<DeleteBucketOutput>> {
        self.bucket(req.credentials.as_ref(), &req.input.bucket, true)?;
        Err(s3_error!(
            NotImplemented,
            "buckets are atlas-native filesystems; remove one from the gateway's bucket list"
        ))
    }

    async fn head_object(
        &self,
        req: S3Request<HeadObjectInput>,
    ) -> S3Result<S3Response<HeadObjectOutput>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, false)?;
        let input = req.input;
        let key = input.key;
        let o = blocking(move || b.head(&key)).await?;
        preconditions(
            &o.etag,
            o.mtime_ns,
            input.if_match.as_ref(),
            input.if_none_match.as_ref(),
            input.if_modified_since.as_ref(),
            input.if_unmodified_since.as_ref(),
        )?;
        Ok(S3Response::new(HeadObjectOutput {
            content_length: Some(o.size as i64),
            e_tag: Some(etag(o.etag.clone())),
            last_modified: Some(timestamp(o.mtime_ns)),
            metadata: metadata(&o.headers),
            content_type: o.headers.content_type,
            content_encoding: o.headers.content_encoding,
            content_disposition: o.headers.content_disposition,
            content_language: o.headers.content_language,
            cache_control: o.headers.cache_control,
            ..Default::default()
        }))
    }

    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, false)?;
        let input = req.input;
        let key = input.key.clone();
        let o = {
            let b = b.clone();
            blocking(move || b.head(&key)).await?
        };
        preconditions(
            &o.etag,
            o.mtime_ns,
            input.if_match.as_ref(),
            input.if_none_match.as_ref(),
            input.if_modified_since.as_ref(),
            input.if_unmodified_since.as_ref(),
        )?;
        let (start, len, content_range) = match &input.range {
            None => (0, o.size, None),
            Some(r) => {
                let r = r.check(o.size)?;
                let len = r.end - r.start;
                (
                    r.start,
                    len,
                    Some(format!("bytes {}-{}/{}", r.start, r.end - 1, o.size)),
                )
            }
        };
        Ok(S3Response::new(GetObjectOutput {
            body: Some(read_stream(b, o.ino, start, len)),
            content_length: Some(len as i64),
            content_range,
            e_tag: Some(etag(o.etag.clone())),
            last_modified: Some(timestamp(o.mtime_ns)),
            metadata: metadata(&o.headers),
            content_type: o.headers.content_type,
            content_encoding: o.headers.content_encoding,
            content_disposition: o.headers.content_disposition,
            content_language: o.headers.content_language,
            cache_control: o.headers.cache_control,
            ..Default::default()
        }))
    }

    async fn put_object(
        &self,
        req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, true)?;
        let trailers = req.trailing_headers;
        let input = req.input;
        if input.if_match.is_some() {
            return Err(s3_error!(
                NotImplemented,
                "If-Match on PUT is not supported"
            ));
        }
        let mut expected = Checksum {
            checksum_crc32: input.checksum_crc32,
            checksum_crc32c: input.checksum_crc32c,
            checksum_crc64nvme: input.checksum_crc64nvme,
            checksum_sha1: input.checksum_sha1,
            checksum_sha256: input.checksum_sha256,
            ..Default::default()
        };
        let hasher = checksum::hasher(
            &expected,
            input.checksum_algorithm.as_ref().map(|a| a.as_str()),
        )?;
        let md5 = content_md5(input.content_md5.as_deref())?;
        let h = headers(
            input.content_type,
            input.content_encoding,
            input.content_disposition,
            input.content_language,
            input.cache_control,
            input.metadata,
        );
        if input.if_none_match.as_ref().is_some_and(|c| c.is_any()) {
            let (b, key) = (b.clone(), input.key.clone());
            match blocking(move || b.head(&key)).await {
                Ok(_) => return Err(s3_error!(PreconditionFailed, "the object exists")),
                Err(e) if *e.code() == S3ErrorCode::NoSuchKey => {}
                Err(e) => return Err(e),
            }
        }
        let mut body = input.body;
        if input.key.ends_with('/') {
            require_empty(body.take()).await?;
        }
        let target = {
            let (b, key, h) = (b.clone(), input.key.clone(), h.clone());
            blocking(move || b.prepare_put(&key, &h)).await?
        };
        let Some((parent, name)) = target else {
            return Ok(S3Response::new(PutObjectOutput {
                e_tag: Some(etag(EMPTY_ETAG.into())),
                ..Default::default()
            }));
        };
        let w = {
            let b = b.clone();
            blocking(move || b.writer(parent)).await?
        };
        let (w, hasher) = write_body(&b, w, hasher, body).await?;
        let actual = hasher.finalize();
        if let Err(e) = checksum::add_trailers(&mut expected, trailers.as_ref())
            .and_then(|()| checksum::verify(&actual, &expected))
        {
            discard(&b, w).await;
            return Err(e);
        }
        let o = blocking(move || b.commit(w, &name, md5, None, h)).await?;
        Ok(S3Response::new(PutObjectOutput {
            e_tag: Some(etag(o.etag)),
            checksum_crc32: actual.checksum_crc32,
            checksum_crc32c: actual.checksum_crc32c,
            checksum_crc64nvme: actual.checksum_crc64nvme,
            checksum_sha1: actual.checksum_sha1,
            checksum_sha256: actual.checksum_sha256,
            ..Default::default()
        }))
    }

    async fn put_object_retention(
        &self,
        req: S3Request<PutObjectRetentionInput>,
    ) -> S3Result<S3Response<PutObjectRetentionOutput>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, true)?;
        let key = req.input.key;
        let Some(ObjectLockRetention {
            mode,
            retain_until_date: Some(date),
        }) = req.input.retention
        else {
            return Err(s3_error!(
                MalformedXML,
                "retention with a RetainUntilDate is required"
            ));
        };
        // Compliance mode only: no role can shorten or remove it, so GOVERNANCE (which the
        // caller could bypass) is refused rather than silently treated as weaker.
        if mode.as_ref().map(|m| m.as_str()) != Some(ObjectLockRetentionMode::COMPLIANCE) {
            return Err(s3_error!(
                InvalidArgument,
                "only Mode=COMPLIANCE retention is supported"
            ));
        }
        let until = time::OffsetDateTime::from(date).unix_timestamp();
        let until = u64::try_from(until)
            .map_err(|_| s3_error!(InvalidArgument, "RetainUntilDate is before 1970"))?;
        blocking(move || b.set_retention(&key, until)).await?;
        Ok(S3Response::new(PutObjectRetentionOutput::default()))
    }

    async fn get_object_retention(
        &self,
        req: S3Request<GetObjectRetentionInput>,
    ) -> S3Result<S3Response<GetObjectRetentionOutput>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, false)?;
        let key = req.input.key;
        let until = blocking(move || b.retention(&key)).await?;
        if until == 0 {
            let mut err = S3Error::with_message(
                S3ErrorCode::Custom("NoSuchObjectLockConfiguration".into()),
                "the object has no retention".to_string(),
            );
            err.set_status_code(http::StatusCode::NOT_FOUND);
            return Err(err);
        }
        let when = time::OffsetDateTime::from_unix_timestamp(until as i64)
            .map_err(|_| s3_error!(InternalError, "bad retention timestamp"))?;
        Ok(S3Response::new(GetObjectRetentionOutput {
            retention: Some(ObjectLockRetention {
                mode: Some(ObjectLockRetentionMode::from_static(
                    ObjectLockRetentionMode::COMPLIANCE,
                )),
                retain_until_date: Some(Timestamp::from(when)),
            }),
        }))
    }

    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, true)?;
        let key = req.input.key;
        blocking(move || b.delete(&key)).await?;
        Ok(S3Response::new(DeleteObjectOutput::default()))
    }

    async fn delete_objects(
        &self,
        req: S3Request<DeleteObjectsInput>,
    ) -> S3Result<S3Response<DeleteObjectsOutput>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, true)?;
        let quiet = req.input.delete.quiet.unwrap_or(false);
        let keys: Vec<String> = req
            .input
            .delete
            .objects
            .into_iter()
            .map(|o| o.key)
            .collect();
        if keys.len() > 1000 {
            return Err(s3_error!(MalformedXML, "at most 1000 keys per request"));
        }
        let results = blocking(move || {
            Ok(keys
                .into_iter()
                .map(|k| (b.delete(&k), k))
                .collect::<Vec<_>>())
        })
        .await?;
        let (mut deleted, mut errors) = (Vec::new(), Vec::new());
        for (r, key) in results {
            match r {
                Ok(()) if !quiet => deleted.push(DeletedObject {
                    key: Some(key),
                    ..Default::default()
                }),
                Ok(()) => {}
                Err(e) => {
                    let e = s3_err(e);
                    errors.push(Error {
                        code: Some(e.code().as_str().to_owned()),
                        key: Some(key),
                        message: e.message().map(str::to_owned),
                        ..Default::default()
                    });
                }
            }
        }
        Ok(S3Response::new(DeleteObjectsOutput {
            deleted: Some(deleted),
            errors: (!errors.is_empty()).then_some(errors),
            ..Default::default()
        }))
    }

    async fn copy_object(
        &self,
        req: S3Request<CopyObjectInput>,
    ) -> S3Result<S3Response<CopyObjectOutput>> {
        let creds = req.credentials.as_ref();
        let input = req.input;
        let (src_bucket, src_key) = source(&input.copy_source)?;
        let src = self.bucket(creds, &src_bucket, false)?;
        let dst = self.bucket(creds, &input.bucket, true)?;
        copy_preconditions(
            &src,
            &src_key,
            input.copy_source_if_match,
            input.copy_source_if_none_match,
            input.copy_source_if_modified_since,
            input.copy_source_if_unmodified_since,
        )
        .await?;
        let replace = input
            .metadata_directive
            .as_ref()
            .is_some_and(|d| d.as_str() == MetadataDirective::REPLACE);
        let new_headers = headers(
            input.content_type,
            input.content_encoding,
            input.content_disposition,
            input.content_language,
            input.cache_control,
            input.metadata,
        );
        let key = input.key;
        let o = if src_bucket == input.bucket && src_key == key {
            if !replace {
                return Err(s3_error!(
                    InvalidRequest,
                    "copying an object onto itself needs the REPLACE metadata directive"
                ));
            }
            blocking(move || dst.set_headers(&key, new_headers)).await?
        } else {
            blocking(move || {
                let s = src.head(&src_key)?;
                let h = if replace {
                    new_headers
                } else {
                    s.headers.clone()
                };
                let Some((parent, name)) = dst.prepare_put(&key, &h)? else {
                    return dst.head(&key);
                };
                let mut w = dst.writer(parent)?;
                if let Err(e) = dst.copy_from(&src, s.ino, 0, s.size, &mut w) {
                    dst.discard(w);
                    return Err(e);
                }
                dst.commit(w, &name, None, None, h)
            })
            .await?
        };
        Ok(S3Response::new(CopyObjectOutput {
            copy_object_result: Some(CopyObjectResult {
                e_tag: Some(etag(o.etag)),
                last_modified: Some(timestamp(o.mtime_ns)),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn list_objects(
        &self,
        req: S3Request<ListObjectsInput>,
    ) -> S3Result<S3Response<ListObjectsOutput>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, false)?;
        let input = req.input;
        let page = list(
            b,
            input.prefix.clone(),
            input.delimiter.clone(),
            input.marker.clone(),
            input.max_keys,
            &input.encoding_type,
        )
        .await?;
        Ok(S3Response::new(ListObjectsOutput {
            name: Some(input.bucket),
            prefix: input.prefix.map(|p| page.enc(p)),
            delimiter: input.delimiter.map(|d| page.enc(d)),
            marker: input.marker.map(|m| page.enc(m)),
            max_keys: Some(page.max as i32),
            encoding_type: input.encoding_type,
            is_truncated: Some(page.truncated),
            next_marker: page
                .next
                .as_ref()
                .filter(|_| page.truncated)
                .map(|k| page.enc(k.clone())),
            contents: page.contents,
            common_prefixes: page.prefixes,
            ..Default::default()
        }))
    }

    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, false)?;
        let input = req.input;
        let token = input
            .continuation_token
            .as_deref()
            .map(decode_token)
            .transpose()?;
        let after = match (token, input.start_after.clone()) {
            (Some(t), Some(s)) => Some(t.max(s)),
            (t, s) => t.or(s),
        };
        let page = list(
            b,
            input.prefix.clone(),
            input.delimiter.clone(),
            after,
            input.max_keys,
            &input.encoding_type,
        )
        .await?;
        let key_count = (page.contents.as_ref().map_or(0, Vec::len)
            + page.prefixes.as_ref().map_or(0, Vec::len)) as i32;
        Ok(S3Response::new(ListObjectsV2Output {
            name: Some(input.bucket),
            prefix: input.prefix.map(|p| page.enc(p)),
            delimiter: input.delimiter.map(|d| page.enc(d)),
            start_after: input.start_after.map(|s| page.enc(s)),
            continuation_token: input.continuation_token,
            next_continuation_token: page
                .next
                .as_ref()
                .filter(|_| page.truncated)
                .map(|k| hex(k.as_bytes())),
            max_keys: Some(page.max as i32),
            key_count: Some(key_count),
            encoding_type: input.encoding_type,
            is_truncated: Some(page.truncated),
            contents: page.contents,
            common_prefixes: page.prefixes,
            ..Default::default()
        }))
    }

    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, true)?;
        let input = req.input;
        let h = headers(
            input.content_type,
            input.content_encoding,
            input.content_disposition,
            input.content_language,
            input.cache_control,
            input.metadata,
        );
        let key = input.key.clone();
        let id = blocking(move || b.create_upload(&key, h)).await?;
        Ok(S3Response::new(CreateMultipartUploadOutput {
            bucket: Some(input.bucket),
            key: Some(input.key),
            upload_id: Some(id),
            ..Default::default()
        }))
    }

    async fn upload_part(
        &self,
        req: S3Request<UploadPartInput>,
    ) -> S3Result<S3Response<UploadPartOutput>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, true)?;
        let trailers = req.trailing_headers;
        let input = req.input;
        let mut expected = Checksum {
            checksum_crc32: input.checksum_crc32,
            checksum_crc32c: input.checksum_crc32c,
            checksum_crc64nvme: input.checksum_crc64nvme,
            checksum_sha1: input.checksum_sha1,
            checksum_sha256: input.checksum_sha256,
            ..Default::default()
        };
        let hasher = checksum::hasher(
            &expected,
            input.checksum_algorithm.as_ref().map(|a| a.as_str()),
        )?;
        let md5 = content_md5(input.content_md5.as_deref())?;
        let (id, key, number) = (input.upload_id, input.key, input.part_number);
        let w = {
            let b = b.clone();
            blocking(move || b.part_writer(&id, &key, number)).await?
        };
        let (w, hasher) = write_body(&b, w, hasher, input.body).await?;
        let actual = hasher.finalize();
        if let Err(e) = checksum::add_trailers(&mut expected, trailers.as_ref())
            .and_then(|()| checksum::verify(&actual, &expected))
        {
            discard(&b, w).await;
            return Err(e);
        }
        let tag = blocking(move || b.commit_part(w, number, md5)).await?;
        Ok(S3Response::new(UploadPartOutput {
            e_tag: Some(etag(tag)),
            checksum_crc32: actual.checksum_crc32,
            checksum_crc32c: actual.checksum_crc32c,
            checksum_crc64nvme: actual.checksum_crc64nvme,
            checksum_sha1: actual.checksum_sha1,
            checksum_sha256: actual.checksum_sha256,
            ..Default::default()
        }))
    }

    async fn upload_part_copy(
        &self,
        req: S3Request<UploadPartCopyInput>,
    ) -> S3Result<S3Response<UploadPartCopyOutput>> {
        let creds = req.credentials.as_ref();
        let input = req.input;
        let (src_bucket, src_key) = source(&input.copy_source)?;
        let src = self.bucket(creds, &src_bucket, false)?;
        let dst = self.bucket(creds, &input.bucket, true)?;
        copy_preconditions(
            &src,
            &src_key,
            input.copy_source_if_match,
            input.copy_source_if_none_match,
            input.copy_source_if_modified_since,
            input.copy_source_if_unmodified_since,
        )
        .await?;
        let (id, key, number, range) = (
            input.upload_id,
            input.key,
            input.part_number,
            input.copy_source_range,
        );
        let (tag, mtime) = blocking(move || {
            let s = src.head(&src_key)?;
            let (offset, len) = copy_range(range.as_deref(), s.size)
                .map_err(|e| StoreError::Invalid(e.to_string()))?;
            let mut w = dst.part_writer(&id, &key, number)?;
            if let Err(e) = dst.copy_from(&src, s.ino, offset, len, &mut w) {
                dst.discard(w);
                return Err(e);
            }
            Ok((dst.commit_part(w, number, None)?, SystemTime::now()))
        })
        .await?;
        Ok(S3Response::new(UploadPartCopyOutput {
            copy_part_result: Some(CopyPartResult {
                e_tag: Some(etag(tag)),
                last_modified: Some(Timestamp::from(mtime)),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn list_parts(
        &self,
        req: S3Request<ListPartsInput>,
    ) -> S3Result<S3Response<ListPartsOutput>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, false)?;
        let input = req.input;
        let (id, key) = (input.upload_id.clone(), input.key.clone());
        let parts = blocking(move || b.list_parts(&id, &key)).await?;
        let after = input.part_number_marker.unwrap_or(0);
        let max = input.max_parts.unwrap_or(1000).clamp(0, 1000) as usize;
        let rest: Vec<_> = parts.into_iter().filter(|p| p.number > after).collect();
        let truncated = rest.len() > max;
        let page: Vec<_> = rest.into_iter().take(max).collect();
        Ok(S3Response::new(ListPartsOutput {
            bucket: Some(input.bucket),
            key: Some(input.key),
            upload_id: Some(input.upload_id),
            part_number_marker: input.part_number_marker,
            next_part_number_marker: page.last().map(|p| p.number).filter(|_| truncated),
            max_parts: Some(max as i32),
            is_truncated: Some(truncated),
            parts: Some(
                page.into_iter()
                    .map(|p| Part {
                        part_number: Some(p.number),
                        size: Some(p.size as i64),
                        e_tag: Some(etag(p.etag)),
                        last_modified: Some(timestamp(p.mtime_ns)),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }))
    }

    async fn list_multipart_uploads(
        &self,
        req: S3Request<ListMultipartUploadsInput>,
    ) -> S3Result<S3Response<ListMultipartUploadsOutput>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, false)?;
        let input = req.input;
        let prefix = input.prefix.clone().unwrap_or_default();
        let uploads = blocking(move || b.list_uploads(&prefix)).await?;
        let max = input.max_uploads.unwrap_or(1000).clamp(0, 1000) as usize;
        let truncated = uploads.len() > max;
        Ok(S3Response::new(ListMultipartUploadsOutput {
            bucket: Some(input.bucket),
            prefix: input.prefix,
            max_uploads: Some(max as i32),
            is_truncated: Some(truncated),
            uploads: Some(
                uploads
                    .into_iter()
                    .take(max)
                    .map(|u| MultipartUpload {
                        key: Some(u.key),
                        upload_id: Some(u.id),
                        initiated: Some(timestamp(u.initiated_ns)),
                        ..Default::default()
                    })
                    .collect(),
            ),
            ..Default::default()
        }))
    }

    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, true)?;
        let input = req.input;
        let parts: Vec<(i32, String)> = input
            .multipart_upload
            .and_then(|m| m.parts)
            .unwrap_or_default()
            .into_iter()
            .map(|p| {
                Ok((
                    p.part_number
                        .ok_or_else(|| s3_error!(InvalidPart, "a part has no number"))?,
                    p.e_tag.map(|e| e.value().to_owned()).unwrap_or_default(),
                ))
            })
            .collect::<S3Result<_>>()?;
        let (bucket, key, id) = (input.bucket, input.key, input.upload_id);
        // Assembling a large object takes a while: s3s keeps the connection alive meanwhile.
        Ok(S3Response::new(CompleteMultipartUploadOutput {
            future: Some(Box::pin(async move {
                let k = key.clone();
                let o = blocking(move || b.complete_upload(&id, &k, &parts)).await?;
                Ok(CompleteMultipartUploadOutput {
                    bucket: Some(bucket),
                    key: Some(key),
                    e_tag: Some(etag(o.etag)),
                    ..Default::default()
                })
            })),
            ..Default::default()
        }))
    }

    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        let b = self.bucket(req.credentials.as_ref(), &req.input.bucket, true)?;
        let (id, key) = (req.input.upload_id, req.input.key);
        blocking(move || b.abort_upload(&id, &key)).await?;
        Ok(S3Response::new(AbortMultipartUploadOutput::default()))
    }
}

/// One listing page, ready for either ListObjects version.
struct Page {
    contents: Option<Vec<Object>>,
    prefixes: Option<Vec<CommonPrefix>>,
    truncated: bool,
    /// The last key or prefix returned (raw).
    next: Option<String>,
    max: usize,
    url: bool,
}

impl Page {
    fn enc(&self, s: String) -> String {
        if self.url {
            url_encode(&s)
        } else {
            s
        }
    }
}

async fn list(
    b: Arc<Bucket>,
    prefix: Option<String>,
    delimiter: Option<String>,
    after: Option<String>,
    max_keys: Option<i32>,
    encoding: &Option<EncodingType>,
) -> S3Result<Page> {
    let max = max_keys.unwrap_or(1000).clamp(0, 1000) as usize;
    let url = encoding
        .as_ref()
        .is_some_and(|e| e.as_str() == EncodingType::URL);
    let prefix = prefix.unwrap_or_default();
    let (items, truncated) =
        blocking(move || b.list(&prefix, delimiter.as_deref(), after.as_deref(), max)).await?;
    let mut page = Page {
        contents: None,
        prefixes: None,
        truncated,
        next: None,
        max,
        url,
    };
    let (mut contents, mut prefixes) = (Vec::new(), Vec::new());
    for item in items {
        match item {
            Listed::Object { key, object } => {
                page.next = Some(key.clone());
                contents.push(Object {
                    key: Some(page.enc(key)),
                    size: Some(object.size as i64),
                    last_modified: Some(timestamp(object.mtime_ns)),
                    e_tag: Some(etag(object.etag)),
                    ..Default::default()
                });
            }
            Listed::Prefix(p) => {
                page.next = Some(p.clone());
                prefixes.push(CommonPrefix {
                    prefix: Some(page.enc(p)),
                });
            }
        }
    }
    page.contents = (!contents.is_empty()).then_some(contents);
    page.prefixes = (!prefixes.is_empty()).then_some(prefixes);
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64_digests() {
        assert_eq!(
            content_md5(Some("kAFQmDzST7DWlj99KOF/cg=="))
                .unwrap()
                .map(|d| hex(&d)),
            Some("900150983cd24fb0d6963f7d28e17f72".to_string())
        );
        assert!(content_md5(Some("not base64!")).is_err());
        assert!(content_md5(Some("AAAA")).is_err());
        assert_eq!(base64_decode("aGk="), Some(b"hi".to_vec()));
    }

    #[test]
    fn encodes_listing_keys_and_tokens() {
        assert_eq!(url_encode("a b/c+d%é"), "a%20b/c%2Bd%25%C3%A9");
        let t = hex("photos/2024/x y".as_bytes());
        assert_eq!(decode_token(&t).unwrap(), "photos/2024/x y");
        assert!(decode_token("zz").is_err());
        assert!(decode_token("abc").is_err());
    }

    #[test]
    fn preconditions_follow_rfc_7232_order() {
        let tag = |s: &str| ETagCondition::ETag(ETag::Strong(s.into()));
        let at = |secs: u64| Timestamp::from(UNIX_EPOCH + Duration::from_secs(secs));
        /// The object: ETag `abc`, modified at 1000.5 s.
        fn check(
            m: Option<&ETagCondition>,
            nm: Option<&ETagCondition>,
            ms: Option<&Timestamp>,
            us: Option<&Timestamp>,
        ) -> Option<S3ErrorCode> {
            preconditions("abc", 1_000_500_000_000, m, nm, ms, us)
                .err()
                .map(|e| e.code().clone())
        }
        assert_eq!(check(None, None, None, None), None);
        assert_eq!(
            check(Some(&tag("x")), None, None, None),
            Some(S3ErrorCode::PreconditionFailed)
        );
        // A matching If-Match overrides If-Unmodified-Since.
        assert_eq!(check(Some(&tag("abc")), None, None, Some(&at(10))), None);
        assert_eq!(
            check(None, None, None, Some(&at(999))),
            Some(S3ErrorCode::PreconditionFailed)
        );
        // Whole seconds: modified at 1000.5 is not after 1000.
        assert_eq!(check(None, None, None, Some(&at(1_000))), None);
        assert_eq!(
            check(None, Some(&tag("abc")), None, None),
            Some(S3ErrorCode::NotModified)
        );
        assert_eq!(
            check(None, Some(&ETagCondition::Any), None, None),
            Some(S3ErrorCode::NotModified)
        );
        // A non-matching If-None-Match overrides If-Modified-Since.
        assert_eq!(check(None, Some(&tag("x")), Some(&at(2_000)), None), None);
        assert_eq!(
            check(None, None, Some(&at(1_000)), None),
            Some(S3ErrorCode::NotModified)
        );
        assert_eq!(check(None, None, Some(&at(999)), None), None);
    }

    #[test]
    fn parses_copy_ranges() {
        assert_eq!(copy_range(None, 10).unwrap(), (0, 10));
        assert_eq!(copy_range(Some("bytes=2-5"), 10).unwrap(), (2, 4));
        assert!(copy_range(Some("bytes=5-10"), 10).is_err());
        assert!(copy_range(Some("bytes=6-5"), 10).is_err());
        assert!(copy_range(Some("2-5"), 10).is_err());
    }
}
