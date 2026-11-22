// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Whole-extent writes place their data without the engine's write lock, so concurrent
//! writers (checkpoint ranks, say) overlap on the data nodes, while partial writes, which merge
//! what they overwrite, stay serialized. Concurrent placements must never share a free range,
//! including space GC frees while they run.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use atlas_native::{
    device::{BlockStore, FileDevice},
    EngineConfig, FailureDomain, MetaBackend, NativeEngine, NativeError, Node,
};

const EXTENT: usize = 64 << 10;

/// Counts writes in flight, holding each long enough for concurrent ones to overlap.
#[derive(Debug)]
struct Counting {
    inner: FileDevice,
    now: AtomicUsize,
    max: Arc<AtomicUsize>,
}

impl Counting {
    fn enter(&self) {
        let n = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(n, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(15));
    }
    fn leave(&self) {
        self.now.fetch_sub(1, Ordering::SeqCst);
    }
}

impl BlockStore for Counting {
    fn append(&self, fence: u64, data: &[u8]) -> Result<u64, NativeError> {
        self.enter();
        let r = BlockStore::append(&self.inner, fence, data);
        self.leave();
        r
    }
    fn write_at(&self, fence: u64, offset: u64, data: &[u8]) -> Result<(), NativeError> {
        self.enter();
        let r = BlockStore::write_at(&self.inner, fence, offset, data);
        self.leave();
        r
    }
    fn read_exact_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, NativeError> {
        self.inner.read_exact_at(offset, len)
    }
    fn len(&self) -> Result<u64, NativeError> {
        BlockStore::len(&self.inner)
    }
}

fn engine(root: &std::path::Path) -> (NativeEngine, Arc<AtomicUsize>) {
    let max = Arc::new(AtomicUsize::new(0));
    let mut c = EngineConfig::new(root);
    c.extent_bytes = EXTENT;
    let stores = (1..=3)
        .map(|i| {
            let node = Node {
                id: format!("n{i}"),
                failure_domain: FailureDomain {
                    zone: "z".into(),
                    rack: format!("r{i}"),
                    host: format!("h{i}"),
                },
                free_bytes: 1 << 30,
                healthy: true,
            };
            let dev = Counting {
                inner: FileDevice::open(root.join(format!("n{i}.data"))).unwrap(),
                now: AtomicUsize::new(0),
                max: max.clone(),
            };
            (node, vec![Arc::new(dev) as Arc<dyn BlockStore>])
        })
        .collect();
    let e = NativeEngine::open_with_devices(c, stores, MetaBackend::Local).unwrap();
    (e, max)
}

fn pattern(writer: usize, round: usize, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| ((i * 31) ^ (i >> 9) ^ (writer * 7 + round * 13)) as u8)
        .collect()
}

#[test]
fn aligned_writers_overlap_and_never_share_space() {
    let td = tempfile::tempdir().unwrap();
    let (e, max) = engine(td.path());
    let vols: Vec<String> = (0..8)
        .map(|i| e.create_volume(format!("v{i}"), 2 * EXTENT as u64).unwrap())
        .collect();
    let done = AtomicBool::new(false);
    thread::scope(|s| {
        // GC frees overwritten extents while the writers run, so placements reuse space.
        s.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                e.gc_once().unwrap();
                thread::sleep(Duration::from_millis(5));
            }
        });
        let writers: Vec<_> = vols
            .iter()
            .enumerate()
            .map(|(w, v)| {
                let e = &e;
                s.spawn(move || {
                    for round in 0..12 {
                        e.write(v, 0, &pattern(w, round, 2 * EXTENT)).unwrap();
                    }
                })
            })
            .collect();
        for h in writers {
            h.join().unwrap();
        }
        done.store(true, Ordering::SeqCst);
    });
    assert!(
        max.load(Ordering::SeqCst) > 1,
        "aligned writes never overlapped on a device"
    );
    for (w, v) in vols.iter().enumerate() {
        assert_eq!(
            e.read(v, 0, 2 * EXTENT).unwrap(),
            pattern(w, 11, 2 * EXTENT),
            "volume {w}"
        );
    }
}

#[test]
fn partial_writes_stay_serialized() {
    let td = tempfile::tempdir().unwrap();
    let (e, max) = engine(td.path());
    let vols: Vec<String> = (0..4)
        .map(|i| e.create_volume(format!("v{i}"), EXTENT as u64).unwrap())
        .collect();
    thread::scope(|s| {
        for (w, v) in vols.iter().enumerate() {
            let e = &e;
            s.spawn(move || {
                for round in 0..4 {
                    e.write(v, 100, &pattern(w, round, 1000)).unwrap();
                }
            });
        }
    });
    // Each extent's replicas go to different devices, so a serialized writer puts one write
    // on a device at a time.
    assert_eq!(max.load(Ordering::SeqCst), 1);
    for (w, v) in vols.iter().enumerate() {
        assert_eq!(e.read(v, 100, 1000).unwrap(), pattern(w, 3, 1000));
    }
}
