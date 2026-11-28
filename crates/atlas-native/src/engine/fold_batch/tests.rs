// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! The rules of one fold batch that need the engine's insides; the pool at work is tested in
//! `tests/delta_folding.rs`.

use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use super::*;
use crate::{
    engine::EngineConfig,
    metadata::DeltaCell,
    metadata::DeltaTarget,
    placement::{FailureDomain, Node},
};

const GRID: usize = 4096;

/// An engine with volume `v` of four cells of 1s.
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
    let e = NativeEngine::open(cfg, nodes).unwrap();
    e.create_volume_as("v".into(), "v", 4 * GRID as u64)
        .unwrap();
    e.write("v", 0, &[1; 4 * GRID]).unwrap();
    e
}

fn cell(n: usize) -> DeltaCell {
    DeltaCell {
        target: DeltaTarget::Volume { id: "v".into() },
        cell: (n * GRID) as u64,
    }
}

fn rewrite(e: &NativeEngine, n: usize) -> Rewrite<'_> {
    e.rewrite_cell(&cell(n)).unwrap().expect("a dirty cell")
}

/// Where a rewrite's copy on node `n1` starts.
fn offset(r: &Rewrite) -> u64 {
    let replicas = &r.fold.extent.replicas;
    replicas.iter().find(|r| r.node_id == "n1").unwrap().offset
}

#[test]
fn a_batch_leaves_out_the_cells_that_changed_and_folds_the_rest() {
    let td = tempfile::tempdir().unwrap();
    let e = engine(td.path());
    for n in 0..3 {
        e.write("v", (n * GRID) as u64 + 8, &[2; 8]).unwrap();
    }
    let rewrites: Vec<Rewrite> = (0..3).map(|n| rewrite(&e, n)).collect();
    // One cell is rewritten whole, another gets one more delta, before the commit.
    e.write("v", GRID as u64, &[3; GRID]).unwrap();
    e.write("v", 2 * GRID as u64 + 100, &[4; 8]).unwrap();
    let applied = e.applied_index().unwrap();
    assert_eq!(
        e.commit_rewrites(&rewrites),
        [FoldEnd::Folded, FoldEnd::Changed, FoldEnd::Folded]
    );
    drop(rewrites);
    assert_eq!(e.applied_index().unwrap(), applied + 1, "one command");
    let t = e.telemetry.snapshot();
    assert_eq!(
        (t.delta_folds, t.delta_folded_deltas, t.delta_fold_conflicts),
        (2, 2, 1)
    );
    let batches = e.telemetry.fold_batch.snapshot();
    assert_eq!((batches.count(), batches.sum), (1, 2));
    assert_eq!(e.telemetry.fold_commit.snapshot().count(), 3);
    // The delta appended after the rewrite stays on the new base.
    assert_eq!(e.pending_deltas().unwrap(), (1, 1));
    let mut want = vec![1; 4 * GRID];
    want[8..16].fill(2);
    want[GRID..2 * GRID].fill(3);
    want[2 * GRID + 8..2 * GRID + 16].fill(2);
    want[2 * GRID + 100..2 * GRID + 108].fill(4);
    e.gc_once().unwrap();
    assert_eq!(e.read("v", 0, want.len()).unwrap(), want);
    e.check_delta_invariants().unwrap();
}

#[test]
fn a_batch_with_nothing_left_to_fold_proposes_nothing() {
    let td = tempfile::tempdir().unwrap();
    let e = engine(td.path());
    e.write("v", 8, &[2; 8]).unwrap();
    let rewrites = [rewrite(&e, 0)];
    e.write("v", 0, &[3; GRID]).unwrap();
    let applied = e.applied_index().unwrap();
    assert_eq!(e.commit_rewrites(&rewrites), [FoldEnd::Changed]);
    assert_eq!(e.applied_index().unwrap(), applied);
    assert_eq!(e.telemetry.fold_batch.snapshot().count(), 0);
}

/// Content written under another term is never proposed and its space is free again; a batch
/// nobody decided (the committer panicked) keeps its space.
#[test]
fn only_an_undecided_batch_keeps_its_space() {
    for undecided in [false, true] {
        let td = tempfile::tempdir().unwrap();
        let e = engine(td.path());
        e.write("v", 8, &[2; 8]).unwrap();
        // Then a free cell-sized range on every node.
        e.write("v", 3 * GRID as u64, &[2; GRID]).unwrap();
        e.gc_once().unwrap();
        let free = e.lease(0).unwrap().take("n1", 0, GRID as u64).unwrap();
        let mut rewrites = [rewrite(&e, 0)];
        assert_eq!(Some(offset(&rewrites[0])), free, "not from free space");
        let applied = e.applied_index().unwrap();
        if undecided {
            let hit = Arc::new(AtomicBool::new(false));
            let flag = hit.clone();
            e.set_delta_hook(Some(Arc::new(move |point| {
                if point == DeltaPoint::FoldBeforePropose {
                    flag.store(true, Ordering::SeqCst);
                    panic!("the committer failed (injected by the test)");
                }
            })));
            let died = catch_unwind(AssertUnwindSafe(|| e.commit_rewrites(&rewrites)));
            assert!(died.is_err() && hit.load(Ordering::SeqCst));
        } else {
            rewrites[0].fence = 7;
            assert_eq!(e.commit_rewrites(&rewrites), [FoldEnd::Failed]);
        }
        drop(rewrites);
        assert_eq!(e.applied_index().unwrap(), applied);
        assert_eq!(e.pending_deltas().unwrap(), (1, 1));
        let next = e.lease(0).unwrap().take("n1", 0, GRID as u64).unwrap();
        assert_eq!(next != free, undecided, "{free:?} then {next:?}");
    }
}

#[test]
fn without_batching_each_fold_is_a_command_of_its_own() {
    let td = tempfile::tempdir().unwrap();
    let mut e = engine(td.path());
    e.cfg.delta_writes.fold_batch_cells = 1;
    assert!(!e.fold_batch_supported().unwrap());
    e.write("v", 8, &[2; 8]).unwrap();
    e.write("v", GRID as u64 + 8, &[2; 8]).unwrap();
    let rewrites = [rewrite(&e, 0), rewrite(&e, 1)];
    let applied = e.applied_index().unwrap();
    assert_eq!(e.commit_rewrites(&rewrites), [FoldEnd::Folded; 2]);
    assert_eq!(e.applied_index().unwrap(), applied + 2);
    assert_eq!(e.telemetry.fold_batch.snapshot().count(), 0);
    drop(rewrites);
    e.cfg.delta_writes.fold_batch_cells = 32;
    e.cfg.delta_writes.enabled = false;
    assert!(!e.fold_batch_supported().unwrap());
}
