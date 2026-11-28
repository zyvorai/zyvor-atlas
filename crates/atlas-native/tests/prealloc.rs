// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Preallocation of file devices (`data_node.preallocate`): what the file means while it runs
//! ahead of the appends, across clean and unclean stops, and under failures.

use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader},
    path::Path,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use atlas_native::{
    device::{DataSync, DeviceSync, FileDevice},
    node::{NativeNode, NodeConfig},
    BlockStore, PreallocConfig, RemoteDevice,
};

const STEP: u64 = 64 << 10;
const AHEAD: u64 = 256 << 10;
const IO_TIMEOUT: Duration = Duration::from_secs(20);
/// The thread name the preparer syncs from.
const PREPARER: &str = "atlas-prealloc";

fn on() -> PreallocConfig {
    PreallocConfig {
        enabled: true,
        ahead_bytes: AHEAD,
        step_bytes: STEP,
    }
}

fn off() -> PreallocConfig {
    PreallocConfig::default()
}

fn open(path: &Path, cfg: PreallocConfig) -> FileDevice {
    FileDevice::open_with_config(path, Box::new(DataSync), cfg).unwrap()
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(2));
    }
}

/// Waits until the preparer is `AHEAD` past the append position of an idle device, with all but
/// at most its last range synced.
fn wait_until_ahead(d: &FileDevice) {
    wait_for("the preparer to get ahead", || {
        let s = d.prealloc_stats().unwrap();
        s.ahead_bytes >= AHEAD && s.prepared_bytes >= AHEAD - 2 * STEP
    });
}

/// Record `i`: its own length and bytes, none of them zero.
fn record(i: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|at| ((i.wrapping_mul(31) + at as u64) % 251) as u8 + 1)
        .collect()
}

#[derive(Default)]
struct Switch {
    held: Mutex<bool>,
    released: Condvar,
    arrived: AtomicBool,
}

impl Switch {
    fn held() -> Arc<Self> {
        let s = Self::default();
        *s.held.lock().unwrap() = true;
        Arc::new(s)
    }
    fn wait(&self) {
        self.arrived.store(true, Ordering::SeqCst);
        let mut held = self.held.lock().unwrap();
        while *held {
            held = self.released.wait(held).unwrap();
        }
    }
    fn release(&self) {
        *self.held.lock().unwrap() = false;
        self.released.notify_all();
    }
    fn wait_for_arrival(&self) {
        wait_for("a sync", || self.arrived.load(Ordering::SeqCst));
    }
}

/// Holds, and optionally fails, the syncs of the preparer or of the writers.
#[derive(Default, Clone)]
struct Syncs {
    hold_preparer: Option<Arc<Switch>>,
    hold_writers: Option<Arc<Switch>>,
    fail_preparer: bool,
    fail_writers: Arc<AtomicBool>,
}

impl std::fmt::Debug for Syncs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Syncs")
    }
}

impl DeviceSync for Syncs {
    fn sync_data(&self, file: &File) -> io::Result<()> {
        let preparer = thread::current().name() == Some(PREPARER);
        let (hold, fail) = if preparer {
            (&self.hold_preparer, self.fail_preparer)
        } else {
            (
                &self.hold_writers,
                self.fail_writers.load(Ordering::SeqCst),
            )
        };
        if let Some(h) = hold {
            h.wait();
        }
        if fail {
            return Err(io::Error::other("injected sync failure"));
        }
        file.sync_data()
    }
}

#[test]
fn off_leaves_the_file_exactly_as_long_as_its_data() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("dev");
    let d = open(&path, off());
    for i in 0..20 {
        d.append(&record(i, 3000)).unwrap();
        assert_eq!(file_len(&path), (i + 1) * 3000);
        assert_eq!(d.len().unwrap(), (i + 1) * 3000);
    }
    assert_eq!(d.prealloc_stats(), None);
    d.seal();
    d.append(b"sealing does nothing here").unwrap();
}

#[test]
fn prepares_ahead_reports_the_used_length_and_cuts_back_on_close() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("dev");
    File::create(&path)
        .and_then(|f| std::os::unix::fs::FileExt::write_all_at(&f, b"existing", 0))
        .unwrap();
    let d = open(&path, on());
    // Nothing is prepared before the first append, so restarts alone lose no space.
    thread::sleep(Duration::from_millis(100));
    assert_eq!(file_len(&path), 8);

    assert_eq!(d.append(&record(0, 4096)).unwrap(), 8);
    wait_until_ahead(&d);
    let used = 8 + 4096;
    assert_eq!(d.len().unwrap(), used);
    assert!(file_len(&path) >= used + AHEAD);
    assert!(file_len(&path) <= used + AHEAD + 2 * STEP + 4096);

    // The prepared space is not part of the device yet.
    assert!(d.read_exact_at(used, 1).is_err());
    assert!(d.read_exact_at(used - 1, 2).is_err());
    assert!(d.write_at(used - 1, b"xx").is_err());
    d.write_at(0, b"EXISTING").unwrap();

    // From the second range on, appends land in prepared space.
    let before = d.prealloc_stats().unwrap().appends_prepared;
    let mut at = used;
    for i in 1..=48 {
        assert_eq!(d.append(&record(i, 4096)).unwrap(), at);
        at += 4096;
    }
    wait_until_ahead(&d);
    assert!(d.prealloc_stats().unwrap().appends_prepared > before);
    assert_eq!(d.len().unwrap(), at);

    drop(d);
    assert_eq!(file_len(&path), at, "a clean close gives the space back");
    let d = open(&path, on());
    assert_eq!(d.read_exact_at(0, 8).unwrap(), b"EXISTING");
    for i in 0..=48 {
        assert_eq!(
            d.read_exact_at(8 + i * 4096, 4096).unwrap(),
            record(i, 4096)
        );
    }
    assert_eq!(d.append(b"next").unwrap(), at);
}

#[test]
fn an_unclean_stop_during_preparation_loses_no_acknowledged_byte() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("dev");
    let hold = Switch::held();
    let syncs = Syncs {
        hold_preparer: Some(hold.clone()),
        ..Default::default()
    };
    let d = FileDevice::open_with_config(&path, Box::new(syncs), on()).unwrap();
    let put = |data: Vec<u8>| (d.append(&data).unwrap(), data);
    let mut acked = vec![put(record(0, 4096))];
    // The preparer has written zeros and is in its sync: appends do not wait for it.
    hold.wait_for_arrival();
    acked.extend((1..10).map(|i| put(record(i, 5000))));
    // Data ending in zeros is why the used length cannot be read back from the bytes.
    acked.push(put(vec![0u8; 4096]));
    let used = d.len().unwrap();

    // What a crash here leaves behind, and what one part way through extending the file would.
    let crashed = fs::read(&path).unwrap();
    assert!(crashed.len() as u64 > used, "nothing was prepared");
    hold.release();
    drop(d);

    let lengths = [
        used as usize,
        used as usize + 1,
        (used as usize + crashed.len()) / 2,
        crashed.len(),
    ];
    for (n, len) in lengths.into_iter().enumerate() {
        for cfg in [off(), on()] {
            let copy = td.path().join(format!("crash-{n}-{}", cfg.enabled));
            fs::write(&copy, &crashed[..len]).unwrap();
            let d = open(&copy, cfg);
            assert_eq!(d.len().unwrap(), len as u64);
            let off = d.append(b"after the crash").unwrap();
            assert_eq!(off, len as u64, "appends go after everything in the file");
            for (at, data) in &acked {
                assert_eq!(&d.read_exact_at(*at, data.len()).unwrap(), data);
            }
            drop(d);
            assert_eq!(file_len(&copy), len as u64 + 15);
        }
    }
}

#[test]
fn turning_it_off_after_an_unclean_stop_and_on_again_is_safe() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("dev");
    let d = open(&path, on());
    d.append(&record(0, 100)).unwrap();
    wait_until_ahead(&d);
    let crashed = td.path().join("crashed");
    fs::copy(&path, &crashed).unwrap();
    drop(d);

    let first = file_len(&crashed);
    let d = open(&crashed, off());
    assert_eq!(d.append(&record(1, 100)).unwrap(), first);
    drop(d);
    let d = open(&crashed, on());
    assert_eq!(d.append(&record(2, 100)).unwrap(), first + 100);
    wait_until_ahead(&d);
    drop(d);
    assert_eq!(file_len(&crashed), first + 200);
    let d = open(&crashed, off());
    for (i, at) in [(0, 0), (1, first), (2, first + 100)] {
        assert_eq!(d.read_exact_at(at, 100).unwrap(), record(i, 100));
    }
}

#[test]
fn a_failed_preparer_sync_fails_the_device_like_any_failed_sync() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("dev");
    let hold = Switch::held();
    let syncs = Syncs {
        hold_preparer: Some(hold.clone()),
        fail_preparer: true,
        ..Default::default()
    };
    let d = FileDevice::open_with_config(&path, Box::new(syncs), on()).unwrap();
    d.append(b"durable").unwrap();
    hold.wait_for_arrival();
    assert_eq!(d.failure(), None);
    hold.release();
    wait_for("the failure", || d.failure().is_some());
    assert!(d.failure().unwrap().contains("injected"));
    assert!(d.append(b"later").is_err());
    assert!(d.write_at(0, b"DUR").is_err());
    assert_eq!(d.read_exact_at(0, 7).unwrap(), b"durable");
    assert_eq!(d.prealloc_stats().unwrap().failures, 1);
    let len = file_len(&path);
    assert!(len > 7);
    drop(d);
    assert_eq!(file_len(&path), len, "a failed device's file is left alone");
    let d = open(&path, on());
    assert_eq!(d.append(b"again").unwrap(), len);
}

#[test]
fn a_writer_in_flight_when_the_preparer_fails_is_not_acknowledged() {
    let td = tempfile::tempdir().unwrap();
    let (preparer, writer) = (Switch::held(), Switch::held());
    let syncs = Syncs {
        hold_preparer: Some(preparer.clone()),
        hold_writers: Some(writer.clone()),
        fail_preparer: true,
        ..Default::default()
    };
    let d = FileDevice::open_with_config(td.path().join("dev"), Box::new(syncs), on()).unwrap();
    thread::scope(|s| {
        let a = s.spawn(|| d.append(b"in flight"));
        writer.wait_for_arrival();
        preparer.wait_for_arrival();
        preparer.release();
        wait_for("the failure", || d.failure().is_some());
        writer.release();
        assert!(a.join().unwrap().is_err());
    });
}

#[test]
fn a_failed_writer_sync_stops_preparation_and_keeps_the_file() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("dev");
    let syncs = Syncs::default();
    let fail = syncs.fail_writers.clone();
    let d = FileDevice::open_with_config(&path, Box::new(syncs), on()).unwrap();
    d.append(b"durable").unwrap();
    wait_until_ahead(&d);
    fail.store(true, Ordering::SeqCst);
    assert!(d.append(&vec![1; 4096]).is_err());
    assert!(d.append(b"later").is_err());
    let len = file_len(&path);
    assert!(len > d.len().unwrap());
    drop(d);
    assert!(file_len(&path) >= len, "a failed device's file was cut");
}

/// Appends of many sizes, several threads, ranges barely larger than the appends: zeros must
/// never land on data.
#[test]
fn concurrent_appends_and_overwrites_survive_preparation() {
    let td = tempfile::tempdir().unwrap();
    for (round, (step, ahead)) in [(4096, 8192), (STEP, AHEAD), (8192, 1 << 20)]
        .into_iter()
        .enumerate()
    {
        let path = td.path().join(format!("dev{round}"));
        let d = open(
            &path,
            PreallocConfig {
                enabled: true,
                ahead_bytes: ahead,
                step_bytes: step,
            },
        );
        let written: Vec<(u64, Vec<u8>)> = thread::scope(|s| {
            let handles: Vec<_> = (0..8u64)
                .map(|t| {
                    let d = &d;
                    s.spawn(move || {
                        let mut mine = Vec::new();
                        for i in 0..150u64 {
                            let seed = t * 1000 + i;
                            let len = 1 + (seed.wrapping_mul(2_654_435_761) % 20_000) as usize;
                            let data = record(seed, len);
                            let off = d.append(&data).unwrap();
                            assert_eq!(d.read_exact_at(off, len).unwrap(), data);
                            mine.push((off, data));
                            if i % 7 == 6 {
                                // Reuse an earlier range, as the engine reuses freed space.
                                let k = (i / 2) as usize;
                                let (off, len) = (mine[k].0, mine[k].1.len());
                                mine[k].1 = record(seed + 7, len);
                                d.write_at(off, &mine[k].1).unwrap();
                            }
                        }
                        mine
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });
        let used: u64 = written.iter().map(|(_, d)| d.len() as u64).sum();
        assert_eq!(d.len().unwrap(), used);
        let stats = d.prealloc_stats().unwrap();
        assert_eq!(stats.appends_prepared + stats.appends_unprepared, 8 * 150);
        assert_eq!(stats.failures, 0);
        let check = |d: &FileDevice| {
            for (off, data) in &written {
                assert_eq!(&d.read_exact_at(*off, data.len()).unwrap(), data);
            }
        };
        check(&d);
        drop(d);
        assert_eq!(file_len(&path), used);
        check(&open(&path, off()));
    }
}

#[test]
fn sealing_under_load_keeps_every_acknowledged_append() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("dev");
    let d = open(&path, on());
    let acked: Vec<(u64, Vec<u8>)> = thread::scope(|s| {
        let handles: Vec<_> = (0..4u64)
            .map(|t| {
                let d = &d;
                s.spawn(move || {
                    let mut mine = Vec::new();
                    for i in 0.. {
                        let data = record(t * 100_000 + i, 3000);
                        match d.append(&data) {
                            Ok(off) => mine.push((off, data)),
                            Err(_) => break,
                        }
                    }
                    mine
                })
            })
            .collect();
        wait_for("preparation", || {
            d.prealloc_stats().unwrap().prepared_bytes >= 4 * STEP
        });
        d.seal();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    assert!(d.append(b"after").is_err());
    let used = d.len().unwrap();
    assert_eq!(file_len(&path), used);
    drop(d);
    assert_eq!(file_len(&path), used);
    let d = open(&path, off());
    for (off, data) in &acked {
        assert_eq!(&d.read_exact_at(*off, data.len()).unwrap(), data);
    }
}

fn data_node_config(dir: &Path, data_node: serde_json::Value) -> NodeConfig {
    serde_json::from_value(serde_json::json!({
        "node_id": "d1",
        "data_dir": dir,
        "http_listen": "127.0.0.1:0",
        "data_node": data_node,
    }))
    .unwrap()
}

#[test]
fn a_data_node_takes_the_setting() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("data/nvme0.data");
    let cfg = data_node_config(
        td.path(),
        serde_json::json!({
            "listen": "127.0.0.1:0",
            "preallocate": {"enabled": true, "ahead_bytes": AHEAD, "step_bytes": STEP},
        }),
    );
    let mut node = NativeNode::start(cfg).unwrap();
    let d = RemoteDevice::new(node.data_node_addr().unwrap(), IO_TIMEOUT);
    assert_eq!(d.append(1, &record(0, 4096)).unwrap(), 0);
    wait_for("preparation", || file_len(&path) > AHEAD);
    assert_eq!(d.len().unwrap(), 4096);
    assert!(d.read_exact_at(4096, 1).is_err());
    node.shutdown();
    assert_eq!(file_len(&path), 4096);

    // Listed devices too, and not the block-aligned backends.
    let listed = |backend: &str, enabled: bool| {
        data_node_config(
            td.path(),
            serde_json::json!({
                "listen": "127.0.0.1:0",
                "devices": [{"path": td.path().join("listed"), "backend": backend}],
                "preallocate": {"enabled": enabled},
            }),
        )
    };
    listed("file", true).validate().unwrap();
    listed("aligned", false).validate().unwrap();
    assert!(listed("aligned", true).validate().is_err());
    assert!(NativeNode::start(listed("aligned", true)).is_err());
}

/// The node binary: `SIGTERM` gives the prepared space back.
#[test]
fn the_node_binary_cuts_its_devices_back_on_sigterm() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("data/nvme0.data");
    let config = td.path().join("node.json");
    let json = serde_json::json!({
        "node_id": "d1",
        "data_dir": td.path(),
        "http_listen": "127.0.0.1:0",
        "data_node": {"listen": "127.0.0.1:0", "preallocate": {"enabled": true}},
    });
    fs::write(&config, json.to_string()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_atlas-native-node"))
        .arg("--config")
        .arg(&config)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
    let addr = stderr
        .by_ref()
        .map(Result::unwrap)
        .find_map(|line| {
            let rest = line.split("data_node=Some(").nth(1)?;
            Some(rest.split(')').next()?.to_string())
        })
        .expect("the node printed no data address");
    let drain = thread::spawn(move || stderr.for_each(drop));
    let d = RemoteDevice::new(addr, IO_TIMEOUT);
    d.append(1, &record(0, 4096)).unwrap();
    wait_for("preparation", || file_len(&path) >= 64 << 20);
    // SAFETY: signals the child this test spawned and has not yet waited for.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let mut status = None;
    wait_for("the node to exit", || {
        status = child.try_wait().unwrap();
        status.is_some()
    });
    drain.join().unwrap();
    assert!(status.unwrap().success(), "{status:?}");
    assert_eq!(file_len(&path), 4096);
}
