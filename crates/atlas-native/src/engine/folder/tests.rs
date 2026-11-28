// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! The fold planner, claims, and the rules that need the engine's insides; the pool at work is
//! tested in `tests/delta_folding.rs`.

use std::collections::BTreeMap;

use super::*;
use crate::{
    engine::{delta::Scope, EngineConfig},
    metadata::DeltaTarget,
    placement::{FailureDomain, Node},
};

const GRID: usize = 4096;
const WAIT: Duration = Duration::from_secs(30);

fn key(volume: &str, cell: u64) -> DeltaCell {
    DeltaCell {
        target: DeltaTarget::Volume { id: volume.into() },
        cell,
    }
}

/// Plans over `dirty`, everything with `after_deltas` or more being due.
fn plan(p: &mut Planner, dirty: &BTreeMap<DeltaCell, u32>, now: Instant, after_deltas: u32) {
    plan_busy(p, dirty, now, after_deltas, &HashSet::new());
}

fn plan_busy(
    p: &mut Planner,
    dirty: &BTreeMap<DeltaCell, u32>,
    now: Instant,
    after_deltas: u32,
    busy: &HashSet<DeltaCell>,
) {
    let rules = Rules {
        now,
        all: false,
        after_deltas,
        after: Duration::from_secs(3600),
        busy,
    };
    let mut pass = p.begin();
    // In two chunks, as the engine reads the catalog.
    let half = dirty.len() / 2;
    p.look(
        &mut pass,
        &rules,
        dirty.iter().take(half).map(|(k, n)| (k, *n)),
    );
    p.look(
        &mut pass,
        &rules,
        dirty.iter().skip(half).map(|(k, n)| (k, *n)),
    );
    p.end(pass, now);
}

fn pick(p: &mut Planner) -> Option<DeltaCell> {
    p.pick(|_| Some(())).map(|(k, ())| k)
}

#[test]
fn most_deltas_first_and_every_fourth_pick_the_longest_waiting() {
    let mut p = Planner::default();
    let t0 = Instant::now();
    // The cold cell sorts after the others, so only its age can put it ahead of them.
    let mut dirty = BTreeMap::from([(key("z", 0), 1)]);
    plan(&mut p, &dirty, t0, 1);
    for i in 0..10u32 {
        dirty.insert(key("a", u64::from(i)), 2 + i);
    }
    plan(&mut p, &dirty, t0 + Duration::from_secs(1), 1);
    let picked: Vec<DeltaCell> = (0..5).map(|_| pick(&mut p).unwrap()).collect();
    assert_eq!(
        picked,
        [
            key("a", 9),
            key("a", 8),
            key("a", 7),
            key("z", 0),
            key("a", 6)
        ]
    );
}

#[test]
fn a_cell_with_one_delta_is_not_starved_by_busier_ones() {
    let mut p = Planner::default();
    let t0 = Instant::now();
    let cold = key("z", 0);
    let mut dirty = BTreeMap::from([(cold.clone(), 1)]);
    plan(&mut p, &dirty, t0, 1);
    // Hot cells, more than one plan queues, all dirty again by every plan.
    for i in 0..600 {
        dirty.insert(key("a", i), 30);
    }
    for round in 1..=8u64 {
        plan(&mut p, &dirty, t0 + Duration::from_secs(round), 1);
        // One pick per plan, as when folds are slower than planning: the turn of the longest
        // waiting cell must not be lost with the queues.
        if pick(&mut p).unwrap() == cold {
            assert!(round <= OLDEST_EVERY);
            return;
        }
    }
    panic!("the cold cell was never picked");
}

#[test]
fn a_plan_queues_the_most_urgent_cells_that_are_due_and_free() {
    let mut p = Planner::default();
    let t0 = Instant::now();
    let dirty: BTreeMap<DeltaCell, u32> = (0..1000u32)
        .map(|i| (key("a", u64::from(i)), i % 500))
        .collect();
    let busy = HashSet::from([key("a", 499)]);
    plan_busy(&mut p, &dirty, t0, 2, &busy);
    assert!(p.more);
    assert_eq!((p.most.len(), p.oldest.len()), (PLAN_CELLS, PLAN_CELLS));
    // Cells 999 and 499 hold the most; the busy one is left out.
    assert_eq!(p.most[0], key("a", 999));
    assert_eq!(p.most[1], key("a", 498));
    for k in &p.most {
        assert!(dirty[k] >= 500 - 129, "{k:?} holds {}", dirty[k]);
    }
    // Cells below the threshold are queued in neither lane until they are old enough.
    let few = BTreeMap::from([(key("b", 0), 1), (key("b", 1), 2)]);
    plan(&mut p, &few, t0, 2);
    assert!(!p.more);
    assert_eq!(pick(&mut p), Some(key("b", 1)));
    assert_eq!(pick(&mut p), None);
    // Cells no longer dirty are forgotten.
    assert_eq!(p.seen.len(), 1);
    plan(&mut p, &few, t0 + Duration::from_secs(3601), 2);
    assert_eq!(pick(&mut p), Some(key("b", 1)));
    assert_eq!(pick(&mut p), Some(key("b", 0)));
}

#[test]
fn an_empty_or_old_queue_is_planned_again() {
    let mut p = Planner::default();
    let t0 = Instant::now();
    let idle = Duration::from_millis(200);
    assert!(p.stale(t0, idle));
    let dirty = BTreeMap::from([(key("a", 0), 3)]);
    plan(&mut p, &dirty, t0, 1);
    assert!(!p.stale(t0, idle));
    assert!(p.stale(t0 + REPLAN, idle));
    pick(&mut p).unwrap();
    assert_eq!(pick(&mut p), None);
    assert!(!p.stale(t0 + idle / 2, idle));
    assert!(p.stale(t0 + idle, idle));
    // With cells left out of the last plan, at once.
    let many: BTreeMap<DeltaCell, u32> = (0..PLAN_CELLS as u64 + 1)
        .map(|i| (key("a", i), 3))
        .collect();
    plan(&mut p, &many, t0, 1);
    while pick(&mut p).is_some() {}
    assert!(p.stale(t0, idle));
}

/// Not a check, a measurement: `cargo test --release -- --ignored --nocapture planning_cost`.
#[test]
#[ignore = "prints how long planning takes"]
fn planning_cost_with_many_dirty_cells() {
    let dirty: BTreeMap<DeltaCell, u32> = (0..65_536u64)
        .map(|i| {
            let k = DeltaCell {
                target: DeltaTarget::File {
                    fs: "0f8fad5b-d9cb-469f-a165-70867728950e".into(),
                    ino: i / 64,
                },
                cell: (i % 64) << 17,
            };
            (k, 1 + (i % 31) as u32)
        })
        .collect();
    let mut p = Planner::default();
    for round in ["first", "second", "third"] {
        let start = Instant::now();
        plan(&mut p, &dirty, start, 1);
        println!(
            "{round} plan of {} cells: {:?}",
            dirty.len(),
            start.elapsed()
        );
    }
}

#[test]
fn a_claim_is_exclusive_and_goes_to_who_waits_for_it_first() {
    let folds = Folds::default();
    let (a, b) = (key("a", 0), key("a", 1));
    let held = folds.try_claim(&a).unwrap();
    assert!(folds.try_claim(&a).is_none());
    drop(folds.try_claim(&b).unwrap());
    let got = AtomicBool::new(false);
    thread::scope(|s| {
        let waiter = s.spawn(|| {
            let claim = folds.claim(&a);
            got.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(100));
            drop(claim);
        });
        let deadline = Instant::now() + WAIT;
        while folds.claims().wanted.is_empty() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(50));
        assert!(!got.load(Ordering::SeqCst), "claimed while held");
        // Released while someone waits: not given to a passer-by.
        let mut claims = folds.claims();
        claims.held.remove(&a);
        let wanted = claims.wanted.contains_key(&a);
        drop(claims);
        assert!(wanted);
        assert!(folds.try_claim(&a).is_none());
        std::mem::forget(held);
        folds.released.notify_all();
        waiter.join().unwrap();
    });
    assert!(got.load(Ordering::SeqCst));
    assert!(folds.claims().held.is_empty() && folds.claims().wanted.is_empty());
    assert!(folds.try_claim(&a).is_some());
}

#[test]
fn a_claim_is_released_when_its_holder_panics() {
    let folds = Folds::default();
    let a = key("a", 0);
    let died = catch_unwind(AssertUnwindSafe(|| {
        let _claim = folds.try_claim(&a).unwrap();
        panic!("a fold failed (injected by the test)");
    }));
    assert!(died.is_err());
    assert!(folds.try_claim(&a).is_some());
}

#[test]
fn the_pause_after_failures_doubles_up_to_a_limit() {
    let ms = Duration::from_millis;
    assert_eq!(backoff(ms(200), 1), ms(200));
    assert_eq!(backoff(ms(200), 3), ms(800));
    assert_eq!(backoff(ms(200), 100), MAX_BACKOFF);
    assert_eq!(backoff(MAX_BACKOFF * 3, 4), MAX_BACKOFF * 3);
}

/// An engine with volume `v` of eight cells of 1s.
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
    cfg.delta_writes.fold_after_deltas = 1;
    cfg.delta_writes.fold_interval_ms = 5;
    let e = NativeEngine::open(cfg, nodes).unwrap();
    for id in ["u", "v"] {
        e.create_volume_as(id.into(), id, 8 * GRID as u64).unwrap();
        e.write(id, 0, &vec![1; 8 * GRID]).unwrap();
    }
    e
}

/// The `n`th cell of volume `id`.
fn cell(id: &str, n: usize) -> DeltaCell {
    key(id, (n * GRID) as u64)
}

/// `n` deltas on cell `cell` of volume `id`.
fn dirty(e: &NativeEngine, id: &str, cell: usize, n: usize) {
    for i in 0..n {
        e.write(id, (cell * GRID + 16 * i) as u64, &[2; 8]).unwrap();
    }
}

/// Runs the pool until `done`, failing after [`WAIT`].
fn pool_until(e: &NativeEngine, done: impl Fn() -> bool) {
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        s.spawn(|| run_folders(&[e], &stop, |ok| assert!(ok, "a fold failed")));
        let deadline = Instant::now() + WAIT;
        while !done() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        stop.store(true, Ordering::SeqCst);
    });
    assert!(done(), "the pool never got there");
}

/// Folding never takes part in reclaim: with a reclaim pass stuck, the pool goes on, also when
/// room is short enough for an early reclaim to be wanted.
#[test]
fn folds_do_not_wait_for_reclaim() {
    let td = tempfile::tempdir().unwrap();
    let mut e = engine(td.path());
    for cell in 0..8 {
        dirty(&e, "v", cell, 1);
    }
    e.cfg.delta_writes.fold_headroom_bytes = e.cfg.delta_writes.max_pending_bytes - 2;
    let e = e;
    assert!(e.with_catalog(|c| e.delta_pressure(c)).unwrap());
    let stuck = e.delta.reclaiming.lock().unwrap();
    pool_until(&e, || e.pending_deltas().unwrap() == (0, 0));
    drop(stuck);
    assert_eq!(e.telemetry.snapshot().delta_folds, 8);
    let freed = e.reclaim_if_pressed().unwrap().expect("room is short");
    assert_eq!(freed.reclaimed, 16);
    assert!(e.reclaim_if_pressed().unwrap().is_none(), "nothing is dead");
    assert_eq!(e.read("v", 0, 8).unwrap(), [2; 8]);
}

#[test]
fn dead_space_over_the_whole_budget_pauses_background_folding_only() {
    let td = tempfile::tempdir().unwrap();
    let mut e = engine(td.path());
    dirty(&e, "v", 0, 1);
    dirty(&e, "u", 0, 1);
    // A dead extent, and a budget it already fills.
    e.write("u", GRID as u64, &vec![3; GRID]).unwrap();
    e.cfg.delta_writes.max_pending_bytes = 3 * GRID as u64;
    e.cfg.delta_writes.fold_headroom_bytes = 0;
    assert!(e.next_fold(true).unwrap().is_none());
    assert_eq!(e.fold_next().unwrap(), None);
    assert_eq!(e.pending_deltas().unwrap(), (2, 2));
    // A snapshot folds its volume whatever room there is, and the pool helps with it.
    {
        let _block = e.block(Scope::Volume("v".into())).unwrap();
        let (found, _claim) = e.next_fold(false).unwrap().expect("the blocked cell");
        assert_eq!(found, cell("v", 0));
    }
    e.create_snapshot_as("s".into(), "v", "s").unwrap();
    assert_eq!(e.pending_deltas().unwrap(), (1, 1));
    // Reclaim makes room, and folding goes on.
    assert!(e.reclaim_if_pressed().unwrap().is_some());
    assert_eq!(e.fold_next().unwrap(), Some(true));
}

#[test]
fn a_scope_waiting_for_its_snapshot_is_folded_first() {
    let td = tempfile::tempdir().unwrap();
    let e = engine(td.path());
    dirty(&e, "u", 0, 9);
    dirty(&e, "u", 1, 9);
    dirty(&e, "v", 0, 1);
    dirty(&e, "v", 5, 1);
    let block = e.block(Scope::Volume("v".into())).unwrap();
    let (first, one) = e.next_fold(true).unwrap().unwrap();
    // Claimed: the next one is the scope's other cell.
    let (second, two) = e.next_fold(false).unwrap().unwrap();
    assert_eq!((first, second), (cell("v", 0), cell("v", 5)));
    drop((one, two, block));
    let (next, _claim) = e.next_fold(true).unwrap().unwrap();
    assert_eq!(next, cell("u", 0));
}

#[test]
fn folding_a_held_cell_is_skipped_or_waited_for() {
    let td = tempfile::tempdir().unwrap();
    let e = engine(td.path());
    dirty(&e, "v", 0, 1);
    dirty(&e, "v", 1, 1);
    let cells = vec![cell("v", 0), cell("v", 1)];
    let held = e.delta.folds.try_claim(&cells[0]).unwrap();
    // The planner leaves a held cell alone.
    let (free, claim) = e.next_fold(true).unwrap().unwrap();
    assert_eq!(free, cells[1]);
    assert!(e.next_fold(true).unwrap().is_none());
    drop(claim);
    assert_eq!(e.fold_cells(cells.clone(), false).unwrap(), 1);
    assert_eq!(e.pending_deltas().unwrap(), (1, 1));
    thread::scope(|s| {
        let waiting = s.spawn(|| e.fold_all_deltas().unwrap());
        thread::sleep(Duration::from_millis(200));
        assert!(!waiting.is_finished(), "folded a held cell");
        assert_eq!(e.pending_deltas().unwrap(), (1, 1));
        drop(held);
        assert_eq!(waiting.join().unwrap(), 1);
    });
    assert_eq!(e.pending_deltas().unwrap(), (0, 0));
}
