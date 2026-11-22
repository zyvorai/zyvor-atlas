// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Snapshot export to object storage and import into another cluster: content-addressed blobs
//! shared between exports, tiered extents exported from their objects, verified imports, and
//! deletes that keep blobs other exports still need.

use std::{path::Path, sync::Arc, time::Duration};

use atlas_native::{
    object::{DirObjectStore, ObjectStore},
    EngineConfig, FailureDomain, NativeEngine, NativeError, Node, TierPolicy,
};

const EXTENT: usize = 16 << 10;

fn node(i: usize) -> Node {
    Node {
        id: format!("n{i}"),
        failure_domain: FailureDomain {
            zone: "z1".into(),
            rack: format!("r{i}"),
            host: format!("h{i}"),
        },
        free_bytes: 1 << 30,
        healthy: true,
    }
}

/// A cluster of its own (metadata and data under `root`) sharing `bucket`.
fn cluster(root: &Path, bucket: &Arc<DirObjectStore>) -> NativeEngine {
    let mut c = EngineConfig::new(root);
    c.extent_bytes = EXTENT;
    c.objects = Some(bucket.clone() as Arc<dyn ObjectStore>);
    c.object_prefix = format!("{}/g0/", root.display())
        .trim_start_matches('/')
        .into();
    c.export_prefix = "exports-test/".into();
    NativeEngine::open(c, (1..=3).map(node).collect()).unwrap()
}

/// No two extents of a pattern are alike (identical extents would share one blob).
fn pattern(seed: u8, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| ((i * 7) ^ (i >> 8) ^ (i >> 14)) as u8 ^ seed)
        .collect()
}

fn blobs(s: &DirObjectStore) -> usize {
    s.list("exports-test/blobs/").unwrap().len()
}

#[test]
fn an_export_imports_into_another_cluster() {
    let td = tempfile::tempdir().unwrap();
    let bucket = Arc::new(DirObjectStore::new(td.path().join("bucket")).unwrap());
    let a = cluster(&td.path().join("a"), &bucket);
    let v = a.create_volume("v", 8 * EXTENT as u64).unwrap();
    let mut data = vec![0u8; 8 * EXTENT];
    // Extents 0-2 and 5; the rest is a hole.
    data[..3 * EXTENT].copy_from_slice(&pattern(1, 3 * EXTENT));
    data[5 * EXTENT..6 * EXTENT].copy_from_slice(&pattern(2, EXTENT));
    a.write(&v, 0, &data[..3 * EXTENT]).unwrap();
    a.write(&v, 5 * EXTENT as u64, &data[5 * EXTENT..6 * EXTENT])
        .unwrap();
    let s = a.create_snapshot(&v, "nightly").unwrap();

    let mut seen = Vec::new();
    let st = a
        .export_snapshot(&s, "vol-1", |d, t| {
            seen.push((d, t));
            true
        })
        .unwrap();
    assert_eq!((st.extents, st.uploaded, st.reused), (4, 4, 0), "{st:?}");
    assert_eq!(seen.last(), Some(&(4, 4)));
    let list = a.list_exports().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(
        (list[0].snapshot_name.as_str(), list[0].size_bytes),
        ("nightly", data.len() as u64)
    );

    // A different cluster that only shares the bucket.
    let b = cluster(&td.path().join("b"), &bucket);
    let m = b.read_export("vol-1").unwrap();
    let w = b.create_volume("restored", m.size_bytes).unwrap();
    let bytes = b.import_export("vol-1", &w, |_, _| true).unwrap();
    assert_eq!(bytes, 4 * EXTENT as u64);
    assert_eq!(b.read(&w, 0, data.len()).unwrap(), data);
}

#[test]
fn a_later_export_uploads_only_what_changed() {
    let td = tempfile::tempdir().unwrap();
    let bucket = Arc::new(DirObjectStore::new(td.path().join("bucket")).unwrap());
    let a = cluster(td.path(), &bucket);
    let v = a.create_volume("v", 4 * EXTENT as u64).unwrap();
    a.write(&v, 0, &pattern(3, 4 * EXTENT)).unwrap();
    let s1 = a.create_snapshot(&v, "mon").unwrap();
    a.export_snapshot(&s1, "mon", |_, _| true).unwrap();
    a.write(&v, EXTENT as u64, &pattern(4, EXTENT)).unwrap();
    let s2 = a.create_snapshot(&v, "tue").unwrap();
    let st = a.export_snapshot(&s2, "tue", |_, _| true).unwrap();
    assert_eq!((st.uploaded, st.reused), (1, 3), "{st:?}");
    assert_eq!(blobs(&bucket), 5);

    // Deleting the first export keeps the blobs the second still uses.
    assert_eq!(a.delete_export("mon").unwrap(), 1);
    assert_eq!(blobs(&bucket), 4);
    assert!(matches!(
        a.read_export("mon"),
        Err(NativeError::NotFound(_))
    ));
    assert_eq!(a.delete_export("tue").unwrap(), 4);
    assert_eq!(blobs(&bucket), 0);
}

#[test]
fn deletes_free_no_blobs_while_an_export_is_in_progress() {
    let td = tempfile::tempdir().unwrap();
    let bucket = Arc::new(DirObjectStore::new(td.path().join("bucket")).unwrap());
    let a = cluster(td.path(), &bucket);
    let v = a.create_volume("v", 2 * EXTENT as u64).unwrap();
    a.write(&v, 0, &pattern(7, 2 * EXTENT)).unwrap();
    let s = a.create_snapshot(&v, "s").unwrap();
    a.export_snapshot(&s, "one", |_, _| true).unwrap();
    assert!(bucket.list("exports-test/pending/").unwrap().is_empty());

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let marker = |ms: u64| format!("{{\"updated_ms\":{ms}}}").into_bytes();
    bucket
        .put("exports-test/pending/other", &marker(now))
        .unwrap();
    // Another cluster's export of the same name is refused while it runs.
    assert!(a.export_snapshot(&s, "other", |_, _| true).is_err());
    assert_eq!(a.delete_export("one").unwrap(), 0);
    assert_eq!(blobs(&bucket), 2);
    assert!(a.list_exports().unwrap().is_empty());

    // A marker its exporter stopped refreshing doesn't hold blobs forever.
    bucket
        .put("exports-test/pending/other", &marker(now - 3_600_000))
        .unwrap();
    a.export_snapshot(&s, "two", |_, _| true).unwrap();
    assert_eq!(a.delete_export("two").unwrap(), 2);
    assert_eq!(blobs(&bucket), 0);
}

#[test]
fn tiered_extents_export_from_their_objects() {
    let td = tempfile::tempdir().unwrap();
    let bucket = Arc::new(DirObjectStore::new(td.path().join("bucket")).unwrap());
    let a = cluster(td.path(), &bucket);
    let v = a.create_volume("v", 2 * EXTENT as u64).unwrap();
    let data = pattern(5, 2 * EXTENT);
    a.write(&v, 0, &data).unwrap();
    let s = a.create_snapshot(&v, "s").unwrap();
    let policy = TierPolicy {
        cold_after: Duration::ZERO,
        min_bytes: 0,
    };
    assert_eq!(a.tier_once(&policy, |_| {}, || false).unwrap().tiered, 2);
    a.export_snapshot(&s, "cold", |_, _| true).unwrap();
    let w = a.create_volume("w", 2 * EXTENT as u64).unwrap();
    a.import_export("cold", &w, |_, _| true).unwrap();
    assert_eq!(a.read(&w, 0, data.len()).unwrap(), data);
}

#[test]
fn a_corrupt_blob_fails_the_import_and_stopped_exports_leave_no_manifest() {
    let td = tempfile::tempdir().unwrap();
    let bucket = Arc::new(DirObjectStore::new(td.path().join("bucket")).unwrap());
    let a = cluster(td.path(), &bucket);
    let v = a.create_volume("v", 2 * EXTENT as u64).unwrap();
    a.write(&v, 0, &pattern(6, 2 * EXTENT)).unwrap();
    let s = a.create_snapshot(&v, "s").unwrap();

    assert!(a.export_snapshot(&s, "half", |d, _| d < 1).is_err());
    assert!(a.list_exports().unwrap().is_empty());
    assert_eq!(blobs(&bucket), 1, "the uploaded blob stays for reuse");

    a.export_snapshot(&s, "full", |_, _| true).unwrap();
    let dup = a.export_snapshot(&s, "full", |_, _| true);
    assert!(dup.unwrap_err().to_string().contains("exists"));
    for bad in ["", ".hidden", "a/b", "x y"] {
        assert!(matches!(
            a.export_snapshot(&s, bad, |_, _| true),
            Err(NativeError::Invalid(_))
        ));
    }

    let key = bucket.list("exports-test/blobs/").unwrap().remove(0);
    let mut bytes = bucket.get(&key).unwrap();
    bytes[3] ^= 1;
    bucket.put(&key, &bytes).unwrap();
    let w = a.create_volume("w", 2 * EXTENT as u64).unwrap();
    assert!(matches!(
        a.import_export("full", &w, |_, _| true),
        Err(NativeError::Checksum(_))
    ));
}
