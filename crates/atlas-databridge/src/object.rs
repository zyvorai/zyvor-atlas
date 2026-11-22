// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Object-storage migration — the *object* leg of Atlas DataBridge (AWS S3 → Ceph RGW or another S3-compatible store).
//!
//! Mirrors the database DataBridge shape (discover → copy → verify) but for S3 objects.
//! Reuses [`atlas_driver_rgw::S3Target`] as the byte-mover for BOTH endpoints: the AWS
//! source and the destination (Ceph RGW or another S3-compatible store) are both S3-compatible (SigV4, path-style), so one
//! client type serves both. The copy is:
//!
//!   list source → diff vs dest (full | incremental by key+size) → stream each object,
//!   recording its sha256 → verify every planned key landed on the destination.
//!
//! Credentials are never logged: [`S3Endpoint`] does not derive `Debug` over its secret
//! fields (see [`S3Endpoint::redacted`]).

use anyhow::{Context, Result};
use atlas_driver_rgw::{S3Target, MIN_PART_SIZE};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sqlx::AnyPool;
use std::collections::HashMap;
use tokio::io::{AsyncWrite, AsyncWriteExt};

/// The object-store provider behind an endpoint. The S3-protocol family (AWS S3, Google
/// Cloud Storage via its S3-interoperability HMAC keys, and any S3-compatible store such as
/// MinIO / Wasabi / DigitalOcean Spaces / Ceph RGW) all move bytes through [`S3Target`]
/// today. `AzureBlob` and `Vmware` are recognized so the API/model is multi-cloud from day
/// one, but they require their own connectors (not in this build) — mirroring how the DB
/// DataBridge gates oracle/mongodb/sqlserver behind cargo features.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectProvider {
    Aws,
    Gcs,
    S3Compatible,
    AzureBlob,
    Vmware,
}

impl ObjectProvider {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "aws" | "aws-s3" | "s3" => ObjectProvider::Aws,
            "gcs" | "gcp" | "google" | "google-cloud-storage" => ObjectProvider::Gcs,
            "azure" | "azure-blob" | "abs" => ObjectProvider::AzureBlob,
            "vmware" | "vsphere" | "vsan" => ObjectProvider::Vmware,
            _ => ObjectProvider::S3Compatible,
        }
    }

    /// True when this provider speaks the S3 protocol and can use [`S3Target`] directly.
    pub fn is_s3_protocol(self) -> bool {
        matches!(
            self,
            ObjectProvider::Aws | ObjectProvider::Gcs | ObjectProvider::S3Compatible
        )
    }
}

/// Connection to one S3-compatible endpoint (AWS source, or Ceph RGW / another S3-compatible store destination).
///
/// Deliberately does NOT derive `Debug`/`Serialize` — the access/secret keys must never
/// reach a log line or a persisted record. Use [`redacted`](Self::redacted) for display.
#[derive(Clone, Deserialize)]
pub struct S3Endpoint {
    /// Full endpoint URL, e.g. `https://s3.us-east-1.amazonaws.com` (AWS) or the S3 / RGW URL.
    pub endpoint: String,
    #[serde(default)]
    pub region: String,
    pub bucket: String,
    /// Optional key prefix to scope the migration to a subtree of the bucket.
    #[serde(default)]
    pub prefix: Option<String>,
    pub access_key: String,
    pub secret_key: String,
}

impl S3Endpoint {
    /// Build an [`S3Target`] byte-mover for this endpoint.
    pub fn target(&self) -> Result<S3Target> {
        S3Target::new(
            &self.endpoint,
            &self.region,
            &self.bucket,
            &self.access_key,
            &self.secret_key,
        )
        .with_context(|| format!("build S3 client for {}", self.redacted()))
    }

    /// Credential-free identifier safe to log (`endpoint/bucket[/prefix]`).
    pub fn redacted(&self) -> String {
        match &self.prefix {
            Some(p) if !p.is_empty() => format!("{}/{}/{}", self.endpoint, self.bucket, p),
            _ => format!("{}/{}", self.endpoint, self.bucket),
        }
    }
}

/// How much to copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CopyMode {
    /// Copy every source object, overwriting the destination.
    Full,
    /// Copy only objects missing from the destination or whose size differs.
    Incremental,
}

/// One object selected for copy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlannedObject {
    pub key: String,
    pub size: u64,
}

/// Live progress, emitted after each object copies.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CopyProgress {
    pub objects_total: usize,
    pub objects_done: usize,
    pub bytes_total: u64,
    pub bytes_done: u64,
}

/// Terminal report of a copy run.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CopyReport {
    pub objects_copied: usize,
    pub bytes_copied: u64,
    /// `(key, sha256_hex)` recorded from the source stream — the migration's audit trail.
    pub checksums: Vec<(String, String)>,
    /// True once every planned key was confirmed present on the destination at the right size.
    pub verified: bool,
    pub verify_error: Option<String>,
}

/// Pure copy planner: given source and destination listings (`(key, size)` pairs), decide
/// which objects to copy. No I/O — unit-tested offline.
///
/// * [`CopyMode::Full`] → every source object.
/// * [`CopyMode::Incremental`] → objects absent from the destination, or present but with a
///   different size (a cheap proxy for "changed"; RGW/AWS ETags are not comparable across
///   multipart boundaries, so size is the portable signal for the first slice).
pub fn diff_objects(
    source: &[(String, u64)],
    dest: &[(String, u64)],
    mode: CopyMode,
) -> Vec<PlannedObject> {
    let dest_map: HashMap<&str, u64> = dest.iter().map(|(k, s)| (k.as_str(), *s)).collect();
    source
        .iter()
        .filter(|(k, s)| match mode {
            CopyMode::Full => true,
            CopyMode::Incremental => dest_map.get(k.as_str()).is_none_or(|ds| ds != s),
        })
        .map(|(k, s)| PlannedObject {
            key: k.clone(),
            size: *s,
        })
        .collect()
}

/// A read-only object-store source. The source is pluggable so non-S3 clouds (Azure Blob,
/// native GCS, …) can migrate in without touching the copy/verify logic. The S3-protocol
/// family uses [`S3ObjectSource`]; Azure Blob uses the feature-gated `AzureBlobSource`.
///
/// `stream_to` writes the object to `sink` as it is read (never buffering the whole object)
/// and returns `(bytes, sha256_hex)` — so a multi-GB model shard costs one pipe buffer, not
/// its full size, in memory.
#[async_trait::async_trait]
pub trait ObjectSource: Send + Sync {
    /// List `(key, size)` under an optional prefix.
    async fn list(&self, prefix: Option<&str>) -> Result<Vec<(String, u64)>>;
    /// Stream an object into `sink`, returning `(bytes, sha256_hex)`.
    async fn stream_to(
        &self,
        key: &str,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<(u64, String)>;
}

/// A write destination for the migration (Ceph RGW or another S3-compatible store). Pluggable so the copy loop can be
/// unit-tested against an in-memory sink instead of a live S3 endpoint.
#[async_trait::async_trait]
pub trait ObjectSink: Send + Sync {
    /// Stream an object in from `reader` (a pipe fed by the source), returning
    /// `(bytes, sha256_hex)`. `part_size` is the multipart chunk size.
    async fn put_stream(
        &self,
        key: &str,
        part_size: usize,
        reader: tokio::io::DuplexStream,
    ) -> Result<(u64, String)>;
    /// List destination `(key, size)` for verification.
    async fn list(&self, prefix: Option<&str>) -> Result<Vec<(String, u64)>>;
    /// Make sure the destination container exists (an S3 bucket is created when missing).
    async fn ensure_container(&self) -> Result<()> {
        Ok(())
    }
}

/// S3-protocol source (AWS S3, GCS S3-interop, MinIO/RGW/…): wraps an [`S3Target`].
pub struct S3ObjectSource(pub S3Target);

#[async_trait::async_trait]
impl ObjectSource for S3ObjectSource {
    async fn list(&self, prefix: Option<&str>) -> Result<Vec<(String, u64)>> {
        self.0.list_objects(prefix).await
    }
    async fn stream_to(
        &self,
        key: &str,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<(u64, String)> {
        self.0.get_object_streaming(key, sink).await
    }
}

/// S3-protocol destination (Ceph RGW or another S3-compatible store): wraps an [`S3Target`], streaming via multipart upload.
pub struct S3ObjectSink(pub S3Target);

#[async_trait::async_trait]
impl ObjectSink for S3ObjectSink {
    async fn put_stream(
        &self,
        key: &str,
        part_size: usize,
        reader: tokio::io::DuplexStream,
    ) -> Result<(u64, String)> {
        self.0.put_multipart_streaming(key, part_size, reader).await
    }
    async fn list(&self, prefix: Option<&str>) -> Result<Vec<(String, u64)>> {
        self.0.list_objects(prefix).await
    }
    async fn ensure_container(&self) -> Result<()> {
        if !self.0.bucket_exists().await {
            self.0
                .create_bucket()
                .await
                .context("create destination bucket")?;
        }
        Ok(())
    }
}

/// Migrates objects from any [`ObjectSource`] into any [`ObjectSink`], streaming each object
/// and copying up to `concurrency` objects at once.
pub struct ObjectMigrator {
    source: Box<dyn ObjectSource>,
    sink: Box<dyn ObjectSink>,
    prefix: Option<String>,
}

impl ObjectMigrator {
    /// Build an S3-source → S3-dest migrator from two endpoints.
    pub fn new(source: &S3Endpoint, dest: &S3Endpoint) -> Result<Self> {
        Ok(Self {
            source: Box::new(S3ObjectSource(source.target()?)),
            sink: Box::new(S3ObjectSink(dest.target()?)),
            prefix: source.prefix.clone(),
        })
    }

    /// Build a migrator from an arbitrary source (e.g. Azure Blob) into a S3 / RGW destination.
    pub fn with_source(
        source: Box<dyn ObjectSource>,
        dest: S3Target,
        prefix: Option<String>,
    ) -> Self {
        Self {
            source,
            sink: Box::new(S3ObjectSink(dest)),
            prefix,
        }
    }

    /// Build a migrator from explicit source + sink (used by tests).
    pub fn with_source_sink(
        source: Box<dyn ObjectSource>,
        sink: Box<dyn ObjectSink>,
        prefix: Option<String>,
    ) -> Self {
        Self {
            source,
            sink,
            prefix,
        }
    }

    /// List both sides and compute the copy plan.
    pub async fn plan(&self, mode: CopyMode) -> Result<Vec<PlannedObject>> {
        let src = self
            .source
            .list(self.prefix.as_deref())
            .await
            .context("list source objects")?;
        self.sink
            .ensure_container()
            .await
            .context("prepare destination")?;
        let dst = self
            .sink
            .list(self.prefix.as_deref())
            .await
            .context("list destination objects")?;
        Ok(diff_objects(&src, &dst, mode))
    }

    /// Copy a single object by piping source → dest through a bounded in-memory duplex, so
    /// only ~`part_size` is in flight. Returns `(bytes, sha256_hex)` from the source read.
    async fn copy_one(&self, key: &str, part_size: usize) -> Result<(u64, String)> {
        let part_size = part_size.max(MIN_PART_SIZE);
        let (mut writer, reader) = tokio::io::duplex(part_size * 2);
        let read = async {
            let res = self.source.stream_to(key, &mut writer).await;
            // Signal EOF to the reader whether the read succeeded or failed.
            let _ = writer.shutdown().await;
            res.with_context(|| format!("read source object {key}"))
        };
        let write = async {
            self.sink
                .put_stream(key, part_size, reader)
                .await
                .with_context(|| format!("write destination object {key}"))
        };
        let ((bytes, sha), _dest) = tokio::try_join!(read, write)?;
        Ok((bytes, sha))
    }

    /// Execute `plan`, copying up to `concurrency` objects concurrently and invoking
    /// `on_progress` as each completes, then verify.
    pub async fn run<F>(
        &self,
        plan: &[PlannedObject],
        concurrency: usize,
        part_size: usize,
        mut on_progress: F,
    ) -> Result<CopyReport>
    where
        F: FnMut(&CopyProgress),
    {
        let bytes_total: u64 = plan.iter().map(|p| p.size).sum();
        let mut progress = CopyProgress {
            objects_total: plan.len(),
            bytes_total,
            ..Default::default()
        };
        let mut report = CopyReport::default();

        // Move owned keys into each copy future (borrowing plan items across `buffer_unordered`
        // trips a higher-ranked-lifetime Send-inference limitation in the job worker).
        let keys: Vec<String> = plan.iter().map(|p| p.key.clone()).collect();
        let mut copies = futures_util::stream::iter(keys)
            .map(|key| async move {
                let res = self.copy_one(&key, part_size).await;
                (key, res)
            })
            .buffer_unordered(concurrency.max(1));

        while let Some((key, res)) = copies.next().await {
            let (bytes, sha) = res?;
            report.objects_copied += 1;
            report.bytes_copied += bytes;
            report.checksums.push((key, sha));
            progress.objects_done = report.objects_copied;
            progress.bytes_done = report.bytes_copied;
            on_progress(&progress);
        }

        match self.verify(plan).await {
            Ok(()) => report.verified = true,
            Err(e) => {
                report.verified = false;
                report.verify_error = Some(e.to_string());
            }
        }
        Ok(report)
    }

    /// Confirm every planned key exists on the destination at the expected size.
    async fn verify(&self, plan: &[PlannedObject]) -> Result<()> {
        let dst = self
            .sink
            .list(self.prefix.as_deref())
            .await
            .context("verify: list destination")?;
        let dmap: HashMap<&str, u64> = dst.iter().map(|(k, s)| (k.as_str(), *s)).collect();
        for obj in plan {
            match dmap.get(obj.key.as_str()) {
                Some(&sz) if sz == obj.size => {}
                Some(&sz) => {
                    anyhow::bail!(
                        "size mismatch for {}: expected {}, got {}",
                        obj.key,
                        obj.size,
                        sz
                    )
                }
                None => anyhow::bail!("object missing on destination: {}", obj.key),
            }
        }
        Ok(())
    }
}

/// Resolve S3 credentials from a referenced k8s Secret. Accepts either the plain
/// `access_key`/`secret_key` keys or the AWS-style `AWS_ACCESS_KEY_ID`/
/// `AWS_SECRET_ACCESS_KEY`. Credentials never leave this process and are never logged.
async fn resolve_creds(
    k8s: &atlas_driver_k8s::K8sDriver,
    namespace: &str,
    secret_ref: &str,
) -> Result<(String, String)> {
    let data = k8s
        .get_secret(namespace, secret_ref)
        .await
        .map_err(|e| anyhow::anyhow!("read secret {namespace}/{secret_ref}: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("secret {namespace}/{secret_ref} not found"))?;
    let access = data
        .get("access_key")
        .or_else(|| data.get("AWS_ACCESS_KEY_ID"))
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!("secret {secret_ref} missing access_key/AWS_ACCESS_KEY_ID")
        })?;
    let secret = data
        .get("secret_key")
        .or_else(|| data.get("AWS_SECRET_ACCESS_KEY"))
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!("secret {secret_ref} missing secret_key/AWS_SECRET_ACCESS_KEY")
        })?;
    Ok((access, secret))
}

/// Drive a persisted object-migration record to completion: resolve creds → plan →
/// copy (persisting progress) → verify → finalize. Invoked by the job engine's
/// `JobSpec::ObjectMigrate` arm. Errors are recorded on the record as `failed`.
pub async fn run_migration(
    pool: &AnyPool,
    k8s: Option<std::sync::Arc<atlas_driver_k8s::K8sDriver>>,
    id: &str,
) -> Result<()> {
    use atlas_inventory::databridge::object_migrations as store;

    let rec = store::get(pool, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("object migration {id} not found"))?;

    // Run the copy, recording a failure on the record if anything goes wrong.
    match run_migration_inner(pool, k8s, &rec).await {
        Ok(verified) => {
            let state = if verified { "completed" } else { "failed" };
            store::finish(pool, id, state, verified, None).await?;
            if verified {
                Ok(())
            } else {
                anyhow::bail!("verification failed for object migration {id}")
            }
        }
        Err(e) => {
            // Record the message but not any credential material (errors here never embed creds).
            let _ = store::finish(pool, id, "failed", false, Some(&e.to_string())).await;
            Err(e)
        }
    }
}

async fn run_migration_inner(
    pool: &AnyPool,
    k8s: Option<std::sync::Arc<atlas_driver_k8s::K8sDriver>>,
    rec: &atlas_api_types::ObjectMigration,
) -> Result<bool> {
    use atlas_inventory::databridge::object_migrations as store;

    let source_provider = ObjectProvider::parse(&rec.source_provider);
    let dest_provider = ObjectProvider::parse(&rec.dest_provider);

    // The destination must be an S3-protocol endpoint — Ceph RGW by default, or another
    // S3-compatible target also supported (any `dest_provider` that resolves to `S3Compatible`).
    if !dest_provider.is_s3_protocol() {
        anyhow::bail!(
            "destination provider '{}' must be S3-protocol (e.g. Ceph RGW or another S3-compatible store)",
            rec.dest_provider
        );
    }
    // VMware vSphere/vSAN stores VMs/VMDKs on block storage — it is not an object store, so
    // an object-copy path does not apply. Its volumes migrate via the block (RBD import) leg.
    if source_provider == ObjectProvider::Vmware {
        anyhow::bail!(
            "VMware vSphere/vSAN is not an object store; migrate its volumes/VMs via the block \
             (RBD import) path, not object copy"
        );
    }

    let k8s = k8s.ok_or_else(|| {
        anyhow::anyhow!(
            "object migration requires a Kubernetes driver to resolve credential secrets"
        )
    })?;

    let src_ref = rec
        .source_secret_ref
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("source_secret_ref is required"))?;
    let dst_ref = rec
        .dest_secret_ref
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("dest_secret_ref is required"))?;
    let (src_ak, src_sk) = resolve_creds(&k8s, &rec.secret_namespace, src_ref).await?;
    let (dst_ak, dst_sk) = resolve_creds(&k8s, &rec.secret_namespace, dst_ref).await?;

    // Destination endpoint (Ceph RGW or another S3-compatible store).
    let dest = S3Endpoint {
        endpoint: rec.dest_endpoint.clone(),
        region: rec.dest_region.clone(),
        bucket: rec.dest_bucket.clone(),
        prefix: rec.source_prefix.clone(), // copy preserves keys (incl. prefix) into dest bucket
        access_key: dst_ak,
        secret_key: dst_sk,
    };

    let mode = match rec.mode.as_str() {
        "full" => CopyMode::Full,
        _ => CopyMode::Incremental,
    };

    // Build the migrator. S3-family sources (aws | gcs | s3-compatible) use the S3 path;
    // Azure Blob uses its native, feature-gated connector.
    let migrator = match source_provider {
        ObjectProvider::AzureBlob => {
            #[cfg(feature = "azure-blob")]
            {
                // Azure convention: access_key = storage account name, secret_key = account
                // key, bucket = container name.
                let src = crate::object_azure::AzureBlobSource::new(
                    &src_ak,
                    &src_sk,
                    &rec.source_bucket,
                )?;
                ObjectMigrator::with_source(
                    Box::new(src),
                    dest.target()?,
                    rec.source_prefix.clone(),
                )
            }
            #[cfg(not(feature = "azure-blob"))]
            {
                let _ = (&src_ak, &src_sk); // creds resolved, but the connector isn't compiled
                anyhow::bail!(
                    "azure-blob connector is not compiled in this build; rebuild with \
                     --features atlas-databridge/azure-blob"
                );
            }
        }
        _ => {
            let source = S3Endpoint {
                endpoint: rec.source_endpoint.clone(),
                region: rec.source_region.clone(),
                bucket: rec.source_bucket.clone(),
                prefix: rec.source_prefix.clone(),
                access_key: src_ak,
                secret_key: src_sk,
            };
            ObjectMigrator::new(&source, &dest)?
        }
    };

    store::set_state(pool, &rec.id, "planning").await?;
    let plan = migrator.plan(mode).await?;
    let bytes_total: u64 = plan.iter().map(|p| p.size).sum();
    store::set_totals(pool, &rec.id, plan.len() as i64, bytes_total as i64).await?;

    // Tuning: per-migration record overrides, else env defaults. Concurrency = objects copied
    // at once; part_size = multipart chunk (floored at the 5 MiB S3 minimum).
    let concurrency = rec
        .concurrency
        .filter(|&n| n > 0)
        .map(|n| n as usize)
        .unwrap_or_else(|| env_usize("ATLAS_DATABRIDGE_OBJECT_CONCURRENCY", 6));
    let part_size = rec
        .part_size_mb
        .filter(|&n| n > 0)
        .map(|n| n as usize)
        .unwrap_or_else(|| env_usize("ATLAS_DATABRIDGE_OBJECT_PART_SIZE_MB", 16))
        .saturating_mul(1024 * 1024);

    store::set_started(pool, &rec.id).await?;
    store::set_state(pool, &rec.id, "copying").await?;
    // Persist progress at most every ~2s of wall-time-equivalent (every 16 objects) to keep
    // the record fresh without a DB write per tiny object.
    let started = std::time::Instant::now();
    let mut last_persist = 0usize;
    let report = {
        let pool = pool.clone();
        let id = rec.id.clone();
        migrator
            .run(&plan, concurrency, part_size, |progress| {
                if progress.objects_done - last_persist >= 16
                    || progress.objects_done == progress.objects_total
                {
                    last_persist = progress.objects_done;
                    let pool = pool.clone();
                    let id = id.clone();
                    let done = progress.objects_done as i64;
                    let bytes = progress.bytes_done as i64;
                    // Throughput in MB/s over the copy so far (decimal MB for readability).
                    let secs = started.elapsed().as_secs_f64().max(0.001);
                    let mbps = (progress.bytes_done as f64 / 1_000_000.0) / secs;
                    // Fire-and-forget progress write; a dropped update is re-sent on the next tick.
                    tokio::spawn(async move {
                        let _ = store::set_progress(&pool, &id, done, bytes, mbps).await;
                    });
                }
            })
            .await?
    };

    store::set_state(pool, &rec.id, "verifying").await?;
    let secs = started.elapsed().as_secs_f64().max(0.001);
    let final_mbps = (report.bytes_copied as f64 / 1_000_000.0) / secs;
    store::set_progress(
        pool,
        &rec.id,
        report.objects_copied as i64,
        report.bytes_copied as i64,
        final_mbps,
    )
    .await?;
    Ok(report.verified)
}

/// Parse a positive-usize env var, falling back to `default`.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;

    fn objs(pairs: &[(&str, u64)]) -> Vec<(String, u64)> {
        pairs.iter().map(|(k, s)| (k.to_string(), *s)).collect()
    }

    /// In-memory source: `stream_to` writes the stored bytes into the sink.
    struct FakeSource {
        objects: HashMap<String, Vec<u8>>,
    }

    #[async_trait::async_trait]
    impl ObjectSource for FakeSource {
        async fn list(&self, _prefix: Option<&str>) -> Result<Vec<(String, u64)>> {
            Ok(self
                .objects
                .iter()
                .map(|(k, v)| (k.clone(), v.len() as u64))
                .collect())
        }
        async fn stream_to(
            &self,
            key: &str,
            sink: &mut (dyn AsyncWrite + Unpin + Send),
        ) -> Result<(u64, String)> {
            let data = self.objects.get(key).cloned().unwrap_or_default();
            sink.write_all(&data).await?;
            sink.flush().await.ok();
            Ok((data.len() as u64, format!("srcsha-{key}")))
        }
    }

    /// In-memory sink: `put_stream` drains the reader and records the byte count.
    struct FakeSink {
        received: Arc<Mutex<HashMap<String, u64>>>,
    }

    #[async_trait::async_trait]
    impl ObjectSink for FakeSink {
        async fn put_stream(
            &self,
            key: &str,
            _part_size: usize,
            mut reader: tokio::io::DuplexStream,
        ) -> Result<(u64, String)> {
            let mut buf = Vec::new();
            reader.read_to_end(&mut buf).await?;
            self.received
                .lock()
                .unwrap()
                .insert(key.to_string(), buf.len() as u64);
            Ok((buf.len() as u64, format!("dstsha-{key}")))
        }
        async fn list(&self, _prefix: Option<&str>) -> Result<Vec<(String, u64)>> {
            Ok(self
                .received
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect())
        }
    }

    #[tokio::test]
    async fn streams_and_copies_concurrently() {
        let mut objects = HashMap::new();
        for i in 0..20u64 {
            objects.insert(format!("obj{i}"), vec![(i % 256) as u8; (100 + i) as usize]);
        }
        let received = Arc::new(Mutex::new(HashMap::new()));
        let mig = ObjectMigrator::with_source_sink(
            Box::new(FakeSource {
                objects: objects.clone(),
            }),
            Box::new(FakeSink {
                received: received.clone(),
            }),
            None,
        );

        let plan = mig.plan(CopyMode::Full).await.unwrap();
        assert_eq!(plan.len(), 20);

        let mut max_done = 0usize;
        let report = mig
            .run(&plan, 4, MIN_PART_SIZE, |p| {
                max_done = max_done.max(p.objects_done)
            })
            .await
            .unwrap();

        assert_eq!(report.objects_copied, 20);
        assert!(report.verified, "verify failed: {:?}", report.verify_error);
        assert_eq!(max_done, 20);

        // Every object was piped through and landed at the right size.
        let recv = received.lock().unwrap();
        assert_eq!(recv.len(), 20);
        for (k, v) in &objects {
            assert_eq!(
                recv.get(k),
                Some(&(v.len() as u64)),
                "size mismatch for {k}"
            );
        }
    }

    #[tokio::test]
    async fn incremental_only_copies_changed() {
        let mut objects = HashMap::new();
        objects.insert("a".to_string(), vec![1u8; 10]);
        objects.insert("b".to_string(), vec![2u8; 20]);
        let received = Arc::new(Mutex::new(HashMap::new()));
        // Destination already has "a" at the same size -> only "b" should copy.
        received.lock().unwrap().insert("a".to_string(), 10u64);

        let mig = ObjectMigrator::with_source_sink(
            Box::new(FakeSource { objects }),
            Box::new(FakeSink {
                received: received.clone(),
            }),
            None,
        );
        let plan = mig.plan(CopyMode::Incremental).await.unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].key, "b");

        let report = mig.run(&plan, 2, MIN_PART_SIZE, |_| {}).await.unwrap();
        assert_eq!(report.objects_copied, 1);
        assert!(report.verified);
    }

    #[test]
    fn full_copies_everything() {
        let src = objs(&[("a", 10), ("b", 20)]);
        let dst = objs(&[("a", 10)]); // 'a' already present — full ignores that
        let plan = diff_objects(&src, &dst, CopyMode::Full);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].key, "a");
        assert_eq!(plan[1].key, "b");
    }

    #[test]
    fn incremental_skips_identical() {
        let src = objs(&[("a", 10), ("b", 20)]);
        let dst = objs(&[("a", 10), ("b", 20)]);
        assert!(diff_objects(&src, &dst, CopyMode::Incremental).is_empty());
    }

    #[test]
    fn incremental_copies_missing_and_changed() {
        let src = objs(&[("a", 10), ("b", 20), ("c", 30)]);
        let dst = objs(&[("a", 10), ("b", 99)]); // b changed size, c missing
        let plan = diff_objects(&src, &dst, CopyMode::Incremental);
        let keys: Vec<&str> = plan.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, vec!["b", "c"]);
        assert_eq!(plan[0].size, 20); // planned size is the SOURCE size
    }

    #[test]
    fn empty_source_plans_nothing() {
        let dst = objs(&[("a", 10)]);
        assert!(diff_objects(&[], &dst, CopyMode::Full).is_empty());
        assert!(diff_objects(&[], &dst, CopyMode::Incremental).is_empty());
    }

    #[test]
    fn redacted_never_contains_secrets() {
        let ep = S3Endpoint {
            endpoint: "https://s3.us-east-1.amazonaws.com".into(),
            region: "us-east-1".into(),
            bucket: "datasets".into(),
            prefix: Some("models/".into()),
            access_key: "AKIA_SECRET_ID".into(),
            secret_key: "super/secret/key".into(),
        };
        let r = ep.redacted();
        assert!(!r.contains("AKIA_SECRET_ID"));
        assert!(!r.contains("super/secret/key"));
        assert!(r.contains("datasets"));
    }

    #[test]
    fn copy_mode_serde_roundtrip() {
        assert_eq!(
            serde_json::from_str::<CopyMode>("\"incremental\"").unwrap(),
            CopyMode::Incremental
        );
        assert_eq!(serde_json::to_string(&CopyMode::Full).unwrap(), "\"full\"");
    }
}
