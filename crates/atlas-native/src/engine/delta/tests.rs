// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Delta-write rules that need the engine's insides; the rest is in `tests/delta_writes.rs`.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use super::*;
use crate::{
    engine::{
        combiner::{Batch, Outcome, Pending},
        EngineConfig, NewNode,
    },
    metadata::ReplicaRef,
    namespace::{NodeType, ROOT_INO},
    placement::{FailureDomain, Node},
};

const GRID: usize = 4096;

fn engine(root: &std::path::Path) -> NativeEngine {
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
    let mut cfg = EngineConfig::new(root);
    cfg.extent_bytes = GRID;
    cfg.delta_writes.enabled = true;
    cfg.delta_writes.gc_grace_ms = 0;
    NativeEngine::open(cfg, nodes).unwrap()
}

/// File `a` of filesystem `f` with two cells of 1s.
fn file(e: &NativeEngine) -> u64 {
    e.create_fs_as("f".into(), "f").unwrap();
    let ino = e
        .fs_mknode(
            "f",
            ROOT_INO,
            NewNode {
                name: "a".into(),
                op_id: "a".into(),
                kind: NodeType::File,
                target: None,
                rdev: 0,
                mode: 0o644,
                create_mode: None,
                uid: 0,
                gid: 0,
            },
        )
        .unwrap()
        .ino;
    e.write_file("f", ino, 0, &[1; 2 * GRID]).unwrap();
    ino
}

/// Leaves a free range on every node (an overwritten cell, reclaimed).
fn with_free_space(e: &NativeEngine) -> u64 {
    let ino = file(e);
    e.write_file("f", ino, 0, &[2; GRID]).unwrap();
    e.gc_once().unwrap();
    assert!(e.free_bytes().unwrap() > 0);
    ino
}

fn on(ino: u64) -> Target<'static> {
    Target::File {
        fs: "f",
        ino,
        replica: false,
    }
}

/// A delta of 8 bytes at the start of cell 0, replicated under `lease` but not proposed.
fn replicated(e: &NativeEngine, ino: u64, lease: &super::super::inflight::Lease) -> DeltaItem {
    let extent = e.place_delta(&[9; 8], 0, lease).unwrap().unwrap();
    DeltaItem {
        target: DeltaTarget::File {
            fs: "f".into(),
            ino,
        },
        cell: 0,
        at: 0,
        extent,
        now_ns: 5,
    }
}

#[test]
fn a_delta_from_another_term_or_a_blocked_target_is_not_proposed() {
    let td = tempfile::tempdir().unwrap();
    let e = engine(td.path());
    let ino = file(&e);
    let applied = e.applied_index().unwrap();

    let lease = e.lease(0).unwrap();
    let item = replicated(&e, ino, &lease);
    let got = e.submit_delta(7, item, lease.detach()).unwrap();
    assert_eq!(got, Outcome::Rejected);

    let blocked = e.block(Scope::Fs("f".into())).unwrap();
    let lease = e.lease(0).unwrap();
    let item = replicated(&e, ino, &lease);
    let got = e.submit_delta(0, item, lease.detach()).unwrap();
    assert_eq!(got, Outcome::Rejected);
    // A writer on a blocked target rewrites the cell instead.
    e.write_file("f", ino, 4, &[3; 4]).unwrap();
    assert_eq!(e.telemetry.snapshot().delta_writes, 0);
    assert_eq!(e.telemetry.snapshot().delta_fallbacks, 1);
    drop(blocked);
    assert_eq!(e.applied_index().unwrap(), applied + 1);

    let lease = e.lease(0).unwrap();
    let item = replicated(&e, ino, &lease);
    let got = e.submit_delta(0, item, lease.detach()).unwrap();
    assert_eq!(got, Outcome::Applied);
    assert_eq!(e.read_file("f", ino, 0, 8).unwrap(), [9; 8]);
}

#[test]
fn an_undecided_batch_keeps_its_space_and_a_rejected_one_gives_it_back() {
    let td = tempfile::tempdir().unwrap();
    let e = engine(td.path());
    let ino = with_free_space(&e);
    let take = || {
        let lease = e.lease(0).unwrap();
        let at = lease.take("n1", 0, 8).unwrap();
        (lease, at)
    };
    for (outcomes, kept) in [(Some(vec![Outcome::Rejected]), false), (None, true)] {
        let (lease, at) = take();
        assert!(at.is_some());
        let item = DeltaItem {
            target: DeltaTarget::File {
                fs: "f".into(),
                ino,
            },
            cell: 0,
            at: 0,
            extent: e
                .place_delta(&[9; 8], 0, &e.lease(0).unwrap())
                .unwrap()
                .unwrap(),
            now_ns: 1,
        };
        // Dropped without outcomes: what a panic while committing leaves behind.
        drop(Batch {
            engine: &e,
            entries: vec![Pending {
                ticket: 1,
                fence: 0,
                item,
                lease: lease.detach(),
            }],
            outcomes,
        });
        let (_again, next) = take();
        assert_eq!(next != at, kept, "{at:?} then {next:?}");
    }
}

#[test]
fn a_new_term_does_not_allocate_from_the_old_terms_view() {
    let td = tempfile::tempdir().unwrap();
    let e = engine(td.path());
    with_free_space(&e);
    e.create_volume_as("v".into(), "v", GRID as u64).unwrap();
    let old = e.lease(1).unwrap();
    let first = old.take("n1", 0, 100).unwrap().unwrap();
    // Space another leader allocated meanwhile, which this one learns from the catalog.
    let mut extent = e
        .place_delta(&[9; 8], 0, &e.lease(1).unwrap())
        .unwrap()
        .unwrap();
    extent.delta = false;
    extent.payload_len = 100;
    extent.replicas = vec![ReplicaRef {
        node_id: "n1".into(),
        device_index: 0,
        offset: first + 100,
    }];
    e.commit(
        MetaCommand::InstallExtent {
            volume_id: "v".into(),
            logical_offset: 0,
            extent,
        },
        None,
    )
    .unwrap();
    let new = e.lease(3).unwrap();
    let got = new.take("n1", 0, 100).unwrap().expect("free space");
    // Neither the range still held nor the one allocated meanwhile.
    assert!(
        got + 100 <= first || got >= first + 200,
        "{got} overlaps {first}..{}",
        first + 200
    );
}

#[test]
fn a_read_of_a_cell_that_was_folded_and_reused_looks_it_up_again() {
    let td = tempfile::tempdir().unwrap();
    let e = engine(td.path());
    let ino = file(&e);
    e.write_file("f", ino, 8, &[5; 8]).unwrap();
    let old = e
        .with_catalog(|c| on(ino).cell_view(c, 0))
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(old.deltas.len(), 1);
    assert!(e
        .fold_cell(
            &e.catalog_copy()
                .unwrap()
                .dirty
                .keys()
                .next()
                .unwrap()
                .clone()
        )
        .unwrap());
    e.gc_once().unwrap();
    // Reuse the freed ranges.
    for _ in 0..3 {
        e.write_file("f", ino, GRID as u64, &[7; GRID]).unwrap();
        e.gc_once().unwrap();
    }
    let mut want = vec![1u8; GRID];
    want[8..16].fill(5);
    let (view, bytes) = e.read_view(old.clone(), Some(&on(ino))).unwrap();
    assert_eq!(bytes, want);
    assert!(view != old && view.deltas.is_empty());
    assert!(matches!(
        e.read_view(old, None),
        Err(NativeError::Checksum(_))
    ));
}

/// Delta data goes to the data nodes without the write lock; proposing it takes the lock, so
/// no delta lands between the read and the commit of a write that holds it.
#[test]
fn delta_data_is_replicated_while_the_write_lock_is_held() {
    let td = tempfile::tempdir().unwrap();
    let e = engine(td.path());
    let ino = file(&e);
    let replicated = Arc::new(AtomicBool::new(false));
    let flag = replicated.clone();
    e.set_delta_hook(Some(Arc::new(move |point| {
        if point == DeltaPoint::Replicated {
            flag.store(true, Ordering::SeqCst);
        }
    })));
    let applied = e.applied_index().unwrap();
    let held = e.write_lock.lock().unwrap();
    thread::scope(|s| {
        let writer = s.spawn(|| e.write_file("f", ino, 8, &[5; 8]).map(|_| ()));
        let deadline = Instant::now() + Duration::from_secs(20);
        while !replicated.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the writer waited for the lock");
            thread::sleep(Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(300));
        assert!(!writer.is_finished());
        assert_eq!(
            e.applied_index().unwrap(),
            applied,
            "proposed under the lock"
        );
        drop(held);
        writer.join().unwrap().unwrap();
    });
    assert_eq!(e.telemetry.snapshot().delta_writes, 1);
}
