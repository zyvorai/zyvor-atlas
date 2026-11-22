// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Conformance self-test for an S3-compatible backend (Ceph RGW or BYO S3). Runs the operations Atlas's real
//! workloads depend on — bucket create/delete, small and multipart object upload, streaming
//! download, prefix listing and key-suffix "versioned" retention — against a throwaway bucket on
//! the live server, so they are verified through the console rather than trusted from unit tests.
//! Every step's outcome is reported; a failing step never leaves the throwaway bucket behind.

use std::sync::Arc;

use anyhow::Result;
use atlas_driver_k8s::K8sDriver;
use atlas_driver_rgw::{S3Target, MIN_PART_SIZE};
use serde_json::{json, Value};
use sqlx::AnyPool;

use super::helpers::{build_s3_target_for_backend, sha256_hex};
use super::object::s3_endpoint;
use crate::spec::JobSpec;

struct Steps(Vec<Value>);

impl Steps {
    fn record(&mut self, step: &str, outcome: Result<String>) -> bool {
        let (ok, detail) = match outcome {
            Ok(d) => (true, d),
            Err(e) => (false, format!("{e:#}")),
        };
        self.0.push(json!({ "step": step, "ok": ok, "detail": detail }));
        ok
    }
    fn all_ok(&self) -> bool {
        self.0.iter().all(|s| s["ok"] == true)
    }
}

/// Deterministic, non-repeating-per-part bytes so a mis-ordered or truncated multipart upload
/// changes the hash.
fn test_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + (i >> 8) * 7) % 251) as u8).collect()
}

pub(crate) async fn dispatch_selftest(
    pool: &AnyPool,
    k8s: &Option<Arc<K8sDriver>>,
    spec: JobSpec,
) -> Result<Value> {
    let JobSpec::S3BackendSelfTest {
        backend_id,
        region,
        credentials_namespace,
    } = spec
    else {
        anyhow::bail!("not an s3 self-test spec");
    };
    let k8s = k8s
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no Kubernetes driver — cannot resolve backend credentials"))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let bucket = format!("atlas-selftest-{:x}", nanos & 0xffff_ffff_ffff);
    let endpoint = s3_endpoint();
    let (s3, _) = build_s3_target_for_backend(
        k8s,
        pool,
        &backend_id,
        &credentials_namespace,
        &endpoint,
        &region,
        &bucket,
    )
    .await?;

    let mut steps = Steps(Vec::new());
    run_steps(&s3, &mut steps).await;
    cleanup(&s3).await;

    let report = json!({
        "backend_id": backend_id,
        "endpoint": endpoint,
        "region": if region.is_empty() { "us-east-1" } else { region.as_str() },
        "bucket": bucket,
        "steps": steps.0,
    });
    if steps.all_ok() {
        Ok(report)
    } else {
        anyhow::bail!("backend self-test failed: {report}")
    }
}

async fn run_steps(s3: &S3Target, steps: &mut Steps) {
    if !steps.record("create bucket", s3.create_bucket().await.map(|_| "created".into())) {
        return;
    }
    let exists = s3.bucket_exists().await;
    steps.record(
        "head bucket",
        if exists { Ok("exists".into()) } else { Err(anyhow::anyhow!("HeadBucket says missing")) },
    );

    let small = b"atlas selftest small object".to_vec();
    let put = s3.put_object("selftest/small.txt", small.clone()).await;
    steps.record("put small object", put.map(|_| format!("{} bytes", small.len())));
    let got = s3.get_object("selftest/small.txt").await;
    steps.record(
        "get small object",
        got.and_then(|b| {
            if b == small { Ok("content matches".into()) } else { Err(anyhow::anyhow!("content differs")) }
        }),
    );

    let big = test_payload(2 * MIN_PART_SIZE + 1024 * 1024);
    let want = sha256_hex(&big);
    let up = s3
        .put_multipart_streaming("selftest/big.bin", MIN_PART_SIZE, std::io::Cursor::new(big.clone()))
        .await
        .and_then(|(n, sha)| {
            if n == big.len() as u64 && sha == want {
                Ok(format!("{n} bytes in 3 parts, sha256 matches"))
            } else {
                Err(anyhow::anyhow!("uploaded {n} bytes / sha {sha}, expected {} / {want}", big.len()))
            }
        });
    steps.record("multipart upload (11 MiB)", up);
    let mut sink: Vec<u8> = Vec::new();
    let fetched = s3.get_object_streaming("selftest/big.bin", &mut sink).await;
    let down = fetched.and_then(|(n, sha)| {
        if n == big.len() as u64 && sha == want && sink == big {
            Ok("round-trip identical".to_string())
        } else {
            Err(anyhow::anyhow!("downloaded {n} bytes / sha {sha}, expected {} / {want}", big.len()))
        }
    });
    steps.record("streaming download of multipart object", down);

    let listed = s3.list_objects(Some("selftest/")).await.and_then(|objs| {
        let has = |k: &str, len: usize| objs.iter().any(|(key, sz)| key == k && *sz == len as u64);
        if has("selftest/small.txt", small.len()) && has("selftest/big.bin", big.len()) {
            Ok(format!("{} objects, sizes match", objs.len()))
        } else {
            Err(anyhow::anyhow!("listing incomplete or sizes wrong: {objs:?}"))
        }
    });
    steps.record("list by prefix", listed);

    let mut ver = Ok("3 versions written, oldest pruned, newest 2 remain".to_string());
    for n in 1..=3u32 {
        if let Err(e) = s3
            .put_object(&format!("selftest/ver.db.{n:020}"), format!("v{n}").into_bytes())
            .await
        {
            ver = Err(e);
            break;
        }
    }
    if ver.is_ok() {
        ver = async {
            let mut keys: Vec<String> = s3
                .list_objects(Some("selftest/ver.db."))
                .await?
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            keys.sort();
            anyhow::ensure!(keys.len() == 3, "expected 3 versions, listed {}", keys.len());
            s3.delete_object(&keys[0]).await?;
            let after = s3.list_objects(Some("selftest/ver.db.")).await?;
            anyhow::ensure!(after.len() == 2, "expected 2 versions after prune, listed {}", after.len());
            Ok("3 versions written, oldest pruned, newest 2 remain".to_string())
        }
        .await;
    }
    steps.record("key-suffix versioned upload + prune", ver);

    let refused = s3.delete_bucket().await;
    steps.record(
        "delete of non-empty bucket is refused",
        match refused {
            Err(e) => Ok(format!("refused: {e:#}").chars().take(160).collect()),
            Ok(()) => Err(anyhow::anyhow!("server deleted a non-empty bucket")),
        },
    );

    let mut cleanup_err = None;
    if let Ok(objs) = s3.list_objects(None).await {
        for (k, _) in objs {
            if let Err(e) = s3.delete_object(&k).await {
                cleanup_err = Some(e);
                break;
            }
        }
    }
    let gone = s3.object_exists("selftest/big.bin").await;
    steps.record(
        "delete objects",
        match (cleanup_err, gone) {
            (None, false) => Ok("all objects deleted".into()),
            (Some(e), _) => Err(e),
            (None, true) => Err(anyhow::anyhow!("object still present after delete")),
        },
    );

    let del = s3.delete_bucket().await;
    let still = s3.bucket_exists().await;
    steps.record(
        "delete bucket",
        match (del, still) {
            (Ok(()), false) => Ok("bucket removed".into()),
            (Err(e), _) => Err(e),
            (Ok(()), true) => Err(anyhow::anyhow!("bucket still exists after delete")),
        },
    );
}

/// Best-effort removal of anything a failed run left behind (a no-op after a clean run).
async fn cleanup(s3: &S3Target) {
    if let Ok(objs) = s3.list_objects(None).await {
        for (k, _) in objs {
            let _ = s3.delete_object(&k).await;
        }
    }
    let _ = s3.delete_bucket().await;
}
