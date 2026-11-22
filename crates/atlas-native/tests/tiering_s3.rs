// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Tiering against a real S3-compatible bucket (Ceph RGW, MinIO, ...). Needs the `s3` feature
//! and `ATLAS_NATIVE_S3_ENDPOINT`, `_BUCKET`, `_ACCESS_KEY`, `_SECRET_KEY` (and optionally
//! `_REGION`); without them it passes without doing anything.

#![cfg(feature = "s3")]

use std::{sync::Arc, time::Duration};

use atlas_native::{
    object::{ObjectStore, S3ObjectStore},
    EngineConfig, FailureDomain, NativeEngine, Node, TierPolicy,
};

fn store() -> Option<S3ObjectStore> {
    let var = |k: &str| std::env::var(format!("ATLAS_NATIVE_S3_{k}")).ok();
    let (endpoint, bucket, key, secret) = (
        var("ENDPOINT")?,
        var("BUCKET")?,
        var("ACCESS_KEY")?,
        var("SECRET_KEY")?,
    );
    let region = var("REGION").unwrap_or_default();
    Some(S3ObjectStore::new(&endpoint, &region, &bucket, &key, &secret).unwrap())
}

/// An engine tiering and exporting under a fresh prefix of `store`.
fn engine(
    store: S3ObjectStore,
) -> (
    Arc<dyn ObjectStore>,
    String,
    tempfile::TempDir,
    NativeEngine,
) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let store: Arc<dyn ObjectStore> = Arc::new(store);
    let prefix = format!("atlas-native-test/{}/", uuid::Uuid::new_v4());
    let td = tempfile::tempdir().unwrap();
    let mut c = EngineConfig::new(td.path());
    c.extent_bytes = 1 << 20;
    c.objects = Some(store.clone());
    c.object_prefix = prefix.clone();
    c.export_prefix = prefix.clone();
    let nodes = (1..=3)
        .map(|i| Node {
            id: format!("n{i}"),
            failure_domain: FailureDomain {
                zone: "z".into(),
                rack: format!("r{i}"),
                host: format!("h{i}"),
            },
            free_bytes: 1 << 30,
            healthy: true,
        })
        .collect();
    let e = NativeEngine::open(c, nodes).unwrap();
    (store, prefix, td, e)
}

#[test]
fn extents_tier_to_a_real_bucket_and_read_back() {
    let Some(store) = store() else {
        eprintln!("ATLAS_NATIVE_S3_* not set; skipping");
        return;
    };
    let (store, prefix, _td, e) = engine(store);
    let v = e.create_volume("v", 4 << 20).unwrap();
    let data: Vec<u8> = (0..4u32 << 20).map(|i| (i % 253) as u8).collect();
    e.write(&v, 0, &data).unwrap();

    let policy = TierPolicy {
        cold_after: Duration::ZERO,
        min_bytes: 0,
    };
    let st = e.tier_once(&policy, |_| {}, || false).unwrap();
    assert_eq!(st.tiered, 4, "{st:?}");
    assert_eq!(store.list(&prefix).unwrap().len(), 4);
    assert_eq!(e.read(&v, 0, data.len()).unwrap(), data);

    e.write(&v, 0, &vec![1u8; 4 << 20]).unwrap();
    assert_eq!(e.gc_once().unwrap().reclaimed, 4);
    assert!(
        store.list(&prefix).unwrap().is_empty(),
        "GC deletes the objects"
    );
}

#[test]
fn snapshots_export_to_a_real_bucket_and_import() {
    let Some(store) = store() else {
        eprintln!("ATLAS_NATIVE_S3_* not set; skipping");
        return;
    };
    let (store, prefix, _td, e) = engine(store);
    let v = e.create_volume("v", 4 << 20).unwrap();
    let data: Vec<u8> = (0..4u32 << 20)
        .map(|i| (i % 253) as u8 ^ (i >> 20) as u8)
        .collect();
    e.write(&v, 0, &data).unwrap();
    let s1 = e.create_snapshot(&v, "one").unwrap();
    let st = e.export_snapshot(&s1, "one", |_, _| true).unwrap();
    assert_eq!(st.uploaded, 4, "{st:?}");
    e.write(&v, 0, &vec![9u8; 1 << 20]).unwrap();
    let s2 = e.create_snapshot(&v, "two").unwrap();
    let st = e.export_snapshot(&s2, "two", |_, _| true).unwrap();
    assert_eq!((st.uploaded, st.reused), (1, 3), "{st:?}");

    let w = e.create_volume("w", 4 << 20).unwrap();
    e.import_export("one", &w, |_, _| true).unwrap();
    assert_eq!(e.read(&w, 0, data.len()).unwrap(), data);

    e.delete_export("one").unwrap();
    e.delete_export("two").unwrap();
    assert!(
        store.list(&prefix).unwrap().is_empty(),
        "deleting every export frees every blob"
    );
}
