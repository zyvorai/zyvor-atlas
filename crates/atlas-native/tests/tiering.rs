// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Tiering cold extents to object storage: space comes back from the data nodes, reads come
//! from the object, recent reads keep extents hot, overwrites and snapshots behave, GC and the
//! orphan sweep delete objects, and a corrupt object fails its checksum.

use std::{path::Path, sync::Arc, thread, time::Duration};

use atlas_native::{
    ec::EcScheme,
    object::{DirObjectStore, ObjectStore},
    EngineConfig, FailureDomain, NativeEngine, NativeError, Node, TierPolicy,
};

const EXTENT: usize = 64 << 10;
const PREFIX: &str = "t/g0/";

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

fn open(root: &Path, nodes: usize, erasure: Option<&str>) -> (NativeEngine, Arc<DirObjectStore>) {
    let store = Arc::new(DirObjectStore::new(root.join("bucket")).unwrap());
    let mut c = EngineConfig::new(root.join("meta"));
    c.extent_bytes = EXTENT;
    c.erasure = erasure.map(|s| s.parse::<EcScheme>().unwrap());
    c.erasure_min_bytes = 4096;
    c.objects = Some(store.clone() as Arc<dyn ObjectStore>);
    c.object_prefix = PREFIX.into();
    let e = NativeEngine::open(c, (1..=nodes).map(node).collect()).unwrap();
    (e, store)
}

fn policy(cold_after: Duration) -> TierPolicy {
    TierPolicy {
        cold_after,
        min_bytes: 0,
    }
}

fn tier(e: &NativeEngine, cold_after: Duration) -> atlas_native::TierStats {
    e.tier_once(&policy(cold_after), |_| {}, || false).unwrap()
}

fn pattern(seed: u8, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(13).wrapping_add(seed))
        .collect()
}

fn objects(s: &DirObjectStore) -> usize {
    s.list(PREFIX).unwrap().len()
}

fn metric(e: &NativeEngine, name: &str) -> u64 {
    let m = e.render_metrics().unwrap();
    m.lines()
        .find_map(|l| match l.split_whitespace().collect::<Vec<_>>()[..] {
            [n, v] if n == name => v.parse().ok(),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{name} missing:\n{m}"))
}

#[test]
fn cold_extents_move_to_objects_and_free_their_replicas() {
    let td = tempfile::tempdir().unwrap();
    let (e, store) = open(td.path(), 3, None);
    let v = e.create_volume("v", 4 * EXTENT as u64).unwrap();
    let data = pattern(1, 4 * EXTENT);
    e.write(&v, 0, &data).unwrap();
    let free = e.free_bytes().unwrap();

    let st = tier(&e, Duration::ZERO);
    assert_eq!((st.candidates, st.tiered, st.deferred), (4, 4, 0), "{st:?}");
    assert_eq!(st.bytes, data.len() as u64);
    assert_eq!(objects(&store), 4);
    // Three replicas of every extent are back on the free lists.
    assert_eq!(e.free_bytes().unwrap() - free, 3 * data.len() as u64);
    assert_eq!(e.read(&v, 0, data.len()).unwrap(), data);
    assert_eq!(metric(&e, "atlas_native_tiered_extents"), 4);
    assert_eq!(metric(&e, "atlas_native_object_reads_total"), 4);

    // Nothing is left to tier, and a second pass changes nothing.
    assert_eq!(tier(&e, Duration::ZERO).candidates, 0);
}

#[test]
fn recently_read_or_written_extents_stay_on_the_data_nodes() {
    let td = tempfile::tempdir().unwrap();
    let (e, store) = open(td.path(), 3, None);
    let v = e.create_volume("v", 3 * EXTENT as u64).unwrap();
    e.write(&v, 0, &pattern(2, 2 * EXTENT)).unwrap();
    let cold = Duration::from_millis(400);
    assert_eq!(tier(&e, cold).candidates, 0, "just written");

    thread::sleep(Duration::from_millis(500));
    e.read(&v, 0, 10).unwrap();
    e.write(&v, 2 * EXTENT as u64, &pattern(3, EXTENT)).unwrap();
    let st = tier(&e, cold);
    assert_eq!(
        st.tiered, 1,
        "only the extent neither read nor written: {st:?}"
    );
    assert_eq!(objects(&store), 1);
    assert_eq!(
        e.read(&v, EXTENT as u64, EXTENT).unwrap(),
        pattern(2, 2 * EXTENT)[EXTENT..]
    );
}

#[test]
fn overwrites_go_to_the_data_nodes_and_gc_deletes_dead_objects() {
    let td = tempfile::tempdir().unwrap();
    let (e, store) = open(td.path(), 3, None);
    let v = e.create_volume("v", 2 * EXTENT as u64).unwrap();
    let mut data = pattern(4, 2 * EXTENT);
    e.write(&v, 0, &data).unwrap();
    tier(&e, Duration::ZERO);
    assert_eq!(objects(&store), 2);

    // A partial overwrite reads the rest of the extent from its object.
    e.write(&v, 100, b"hello").unwrap();
    data[100..105].copy_from_slice(b"hello");
    // A whole-extent overwrite needs nothing from the object.
    let fresh = pattern(5, EXTENT);
    e.write(&v, EXTENT as u64, &fresh).unwrap();
    data[EXTENT..].copy_from_slice(&fresh);
    assert_eq!(e.read(&v, 0, data.len()).unwrap(), data);

    // With delta writes on, the partial overwrite is a delta until folded.
    let deltas = e.fold_all_deltas().unwrap() as u64;
    assert_eq!(e.read(&v, 0, data.len()).unwrap(), data);
    let gc = e.gc_once().unwrap();
    assert_eq!(gc.reclaimed, 2 + deltas);
    assert_eq!(objects(&store), 0, "both tiered extents are dead");
    assert_eq!(metric(&e, "atlas_native_tiered_extents"), 0);
}

#[test]
fn a_snapshot_keeps_its_tiered_extents_until_it_is_deleted() {
    let td = tempfile::tempdir().unwrap();
    let (e, store) = open(td.path(), 3, None);
    let v = e.create_volume("v", EXTENT as u64).unwrap();
    let old = pattern(6, EXTENT);
    e.write(&v, 0, &old).unwrap();
    let snap = e.create_snapshot(&v, "s").unwrap();
    tier(&e, Duration::ZERO);
    e.write(&v, 0, &pattern(7, EXTENT)).unwrap();
    e.gc_once().unwrap();
    assert_eq!(objects(&store), 1, "the snapshot still needs it");
    assert_eq!(e.read_snapshot(&snap, 0, EXTENT).unwrap(), old);

    e.delete_snapshot(&snap).unwrap();
    e.gc_once().unwrap();
    assert_eq!(objects(&store), 0);
}

#[test]
fn orphans_are_swept_and_a_corrupt_object_fails_its_checksum() {
    let td = tempfile::tempdir().unwrap();
    let (e, store) = open(td.path(), 3, None);
    let v = e.create_volume("v", EXTENT as u64).unwrap();
    e.write(&v, 0, &pattern(8, EXTENT)).unwrap();
    store
        .put(&format!("{PREFIX}extents/lost-upload"), b"x")
        .unwrap();
    store.put("t/g1/extents/another-group", b"x").unwrap();

    let st = tier(&e, Duration::ZERO);
    assert_eq!((st.tiered, st.orphans_deleted), (1, 1), "{st:?}");
    assert_eq!(
        store.list("t/g1/").unwrap().len(),
        1,
        "other prefixes are left alone"
    );

    let key = store.list(PREFIX).unwrap().remove(0);
    let mut bytes = store.get(&key).unwrap();
    bytes[0] ^= 0xFF;
    store.put(&key, &bytes).unwrap();
    assert!(matches!(e.read(&v, 0, 16), Err(NativeError::Checksum(_))));
}

#[test]
fn erasure_coded_extents_tier_and_free_every_shard() {
    let td = tempfile::tempdir().unwrap();
    let (e, store) = open(td.path(), 6, Some("4+2"));
    let v = e.create_volume("v", 2 * EXTENT as u64).unwrap();
    let data = pattern(9, 2 * EXTENT);
    e.write(&v, 0, &data).unwrap();
    let free = e.free_bytes().unwrap();
    assert_eq!(tier(&e, Duration::ZERO).tiered, 2);
    assert_eq!(objects(&store), 2);
    // Six shards of a quarter extent each, per extent.
    assert_eq!(e.free_bytes().unwrap() - free, 2 * 6 * (EXTENT as u64 / 4));
    assert_eq!(e.read(&v, 0, data.len()).unwrap(), data);
}

#[test]
fn without_an_object_store_nothing_tiers() {
    let td = tempfile::tempdir().unwrap();
    let mut c = EngineConfig::new(td.path());
    c.extent_bytes = EXTENT;
    let e = NativeEngine::open(c, (1..=3).map(node).collect()).unwrap();
    let v = e.create_volume("v", EXTENT as u64).unwrap();
    e.write(&v, 0, &pattern(1, EXTENT)).unwrap();
    let id = e.cold_extents(&policy(Duration::ZERO)).unwrap().remove(0);
    assert!(matches!(e.tier_extent(&id), Err(NativeError::Invalid(_))));
}
