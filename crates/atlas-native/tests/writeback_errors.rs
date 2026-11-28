// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Writeback errors from a real Linux kernel: writes to chosen blocks of a device fail while the
//! test runs. Every test is ignored unless `deploy/native/writeback-error-lab.sh` runs it: the
//! script builds the failing device (a device-mapper target on a loop device over its own file)
//! and serves the test's requests to fail or heal blocks.
//!
//! Environment, set by the script:
//! - `ATLAS_WB_DIR`: a directory on the filesystem on the failing device.
//! - `ATLAS_WB_CTL`: a directory elsewhere, for requests to the script and the manifest of
//!   ranges the script checks again after remounting.
//! - `ATLAS_WB_TRIALS` (default 300): rounds of the unordered races.
#![cfg(target_os = "linux")]

use std::{
    fs::{self, File, OpenOptions},
    io,
    os::{fd::AsRawFd, unix::fs::FileExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{channel, Receiver, Sender},
        Arc, Barrier, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use atlas_native::{
    device::{DeviceSync, FileDevice},
    raw::{PreadIo, RawDevice},
    store::CatalogStore,
    wal::{Wal, WalRecord},
    BlockStore, Catalog, MetaCommand, RaftConfig, RaftNode,
};

const WAIT: Duration = Duration::from_secs(60);
const BLOCK: usize = 4096;
/// Where writer A writes, and where writer B writes: B's blocks fail.
const A_AT: u64 = 0;
const B_AT: u64 = 1 << 20;
const FILE_LEN: usize = 2 << 20;
const OLD: u8 = b'o';

fn dir(var: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(var).unwrap_or_else(|| {
        panic!("{var} is not set: run these through deploy/native/writeback-error-lab.sh")
    }))
}

/// Asks the script to change which blocks fail, and waits until it has.
fn request(line: String) {
    if let Err(e) = try_request(&line) {
        panic!("request `{line}`: {e}");
    }
}

fn try_request(line: &str) -> Result<(), String> {
    let ctl = dir("ATLAS_WB_CTL");
    let reply = ctl.join("reply");
    let _ = fs::remove_file(&reply);
    fs::write(ctl.join("request.tmp"), format!("{line}\n")).unwrap();
    fs::rename(ctl.join("request.tmp"), ctl.join("request")).unwrap();
    let deadline = Instant::now() + WAIT;
    while !reply.exists() {
        if Instant::now() >= deadline {
            return Err("no reply".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    match fs::read_to_string(&reply).unwrap().trim() {
        "ok" => Ok(()),
        other => Err(other.into()),
    }
}

/// Writes to `[offset, offset + len)` of `path` fail from now on. The range must be on disk.
fn fail_blocks(path: &Path, offset: u64, len: u64) {
    request(format!("fail {} {offset} {len}", path.display()));
}

/// Writes to every block `path` has now fail.
fn fail_file(path: &Path) {
    request(format!("fail-file {}", path.display()));
}

fn heal() {
    request("heal".into());
}

/// Heals the device when a test ends, however it ends.
struct Faults;

impl Drop for Faults {
    fn drop(&mut self) {
        // Not a panic: this also runs while a failed test unwinds.
        if let Err(e) = try_request("heal") {
            eprintln!("could not heal the device: {e}");
        }
    }
}

fn drop_cache(path: &Path) {
    let f = File::open(path).unwrap();
    // SAFETY: the descriptor is open for the call, which touches no memory.
    let rc = unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    assert_eq!(rc, 0);
}

/// The bytes the disk holds, as opposed to the page cache.
fn on_disk(path: &Path, offset: u64, len: usize) -> Vec<u8> {
    drop_cache(path);
    cached(path, offset, len)
}

fn cached(path: &Path, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0; len];
    File::open(path)
        .unwrap()
        .read_exact_at(&mut buf, offset)
        .unwrap();
    buf
}

fn describe(bytes: &[u8]) -> String {
    match bytes.first() {
        Some(&b) if bytes.iter().all(|&x| x == b) => format!("all {:?}", b as char),
        _ => "mixed".into(),
    }
}

/// A file of old data, entirely on disk and out of the page cache.
fn old_file(name: &str) -> PathBuf {
    let path = dir("ATLAS_WB_DIR").join(name);
    let f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .read(true)
        .open(&path)
        .unwrap();
    f.write_all_at(&vec![OLD; FILE_LEN], 0).unwrap();
    f.sync_all().unwrap();
    File::open(dir("ATLAS_WB_DIR")).unwrap().sync_all().unwrap();
    drop_cache(&path);
    path
}

/// Records that B's range of `path` must still hold the old data after a remount.
fn expect_old_after_remount(path: &Path) {
    use std::io::Write;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir("ATLAS_WB_CTL").join("manifest"))
        .unwrap();
    writeln!(f, "{}", path.display()).unwrap();
}

/// Run by the script after it has remounted the filesystem.
#[test]
#[ignore = "needs deploy/native/writeback-error-lab.sh"]
fn verify_after_remount() {
    let manifest = fs::read_to_string(dir("ATLAS_WB_CTL").join("manifest")).unwrap();
    for path in manifest.lines() {
        let got = cached(Path::new(path), B_AT, BLOCK);
        println!("after remount, B's block of {path}: {}", describe(&got));
        assert_eq!(got, vec![OLD; BLOCK], "{path}");
    }
}

/// The kernel on its own, without any of this crate: the premise of the fix.
#[test]
#[ignore = "needs deploy/native/writeback-error-lab.sh"]
fn kernel_reports_a_writeback_error_to_one_sync_only() {
    let path = old_file("kernel.data");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let _faults = Faults;
    fail_blocks(&path, B_AT, BLOCK as u64);
    file.write_all_at(&[b'b'; BLOCK], B_AT).unwrap();
    file.write_all_at(&[b'a'; BLOCK], A_AT).unwrap();
    let first = file.sync_data();
    let second = file.sync_data();
    let reopened = File::open(&path).unwrap().sync_data();
    let in_cache = cached(&path, B_AT, BLOCK);
    heal();
    let healed = file.sync_data();
    let disk = on_disk(&path, B_AT, BLOCK);
    println!("first fdatasync: {first:?}");
    println!("second fdatasync, same open file: {second:?}");
    println!("fdatasync of a newly opened file: {reopened:?}");
    println!("fdatasync after the device healed: {healed:?}");
    println!(
        "B's block in the page cache: {}; on disk: {}",
        describe(&in_cache),
        describe(&disk)
    );
    assert!(first.is_err());
    assert!(second.is_ok() && reopened.is_ok() && healed.is_ok());
    assert_eq!(in_cache, vec![b'b'; BLOCK]);
    assert_eq!(disk, vec![OLD; BLOCK]);
    expect_old_after_remount(&path);
}

/// Orders two real `fdatasync` calls: A's runs first and is the one told of the error, and it
/// returns to the device only after B's has returned.
#[derive(Debug, Clone)]
struct Ordered {
    b_wrote: Arc<(Mutex<Receiver<()>>, Sender<()>)>,
    a_synced: Arc<(Mutex<Receiver<()>>, Sender<()>)>,
    b_synced: Arc<(Mutex<Receiver<()>>, Sender<()>)>,
    results: Arc<Mutex<Vec<String>>>,
}

impl Ordered {
    fn new() -> Self {
        let pair = || {
            let (tx, rx) = channel();
            Arc::new((Mutex::new(rx), tx))
        };
        Self {
            b_wrote: pair(),
            a_synced: pair(),
            b_synced: pair(),
            results: Arc::default(),
        }
    }
}

fn signal(p: &(Mutex<Receiver<()>>, Sender<()>)) {
    p.1.send(()).unwrap();
}

fn await_signal(p: &(Mutex<Receiver<()>>, Sender<()>)) {
    p.0.lock().unwrap().recv_timeout(WAIT).unwrap();
}

impl DeviceSync for Ordered {
    fn sync_data(&self, file: &File) -> io::Result<()> {
        let a = thread::current().name() == Some("writer-a");
        if a {
            await_signal(&self.b_wrote);
        } else {
            signal(&self.b_wrote);
            await_signal(&self.a_synced);
        }
        let result = file.sync_data();
        self.results.lock().unwrap().push(format!(
            "{}'s fdatasync: {result:?}",
            if a { "A" } else { "B" }
        ));
        if a {
            signal(&self.a_synced);
            await_signal(&self.b_synced);
            // A holds the error; B has long since had its success.
            thread::sleep(Duration::from_millis(200));
        } else {
            signal(&self.b_synced);
        }
        result
    }
}

#[test]
#[ignore = "needs deploy/native/writeback-error-lab.sh"]
fn file_device_does_not_acknowledge_a_write_the_disk_never_got() {
    let path = old_file("ordered.data");
    let order = Ordered::new();
    let d = FileDevice::open_with_sync(&path, Box::new(order.clone())).unwrap();
    let _faults = Faults;
    fail_blocks(&path, B_AT, BLOCK as u64);
    let d = &d;
    let (a, b) = thread::scope(|s| {
        let a = thread::Builder::new()
            .name("writer-a".into())
            .spawn_scoped(s, move || d.write_at(A_AT, &[b'a'; BLOCK]))
            .unwrap();
        let b = thread::Builder::new()
            .name("writer-b".into())
            .spawn_scoped(s, move || d.write_at(B_AT, &[b'b'; BLOCK]))
            .unwrap();
        (a.join().unwrap(), b.join().unwrap())
    });
    heal();
    for line in order.results.lock().unwrap().iter() {
        println!("{line}");
    }
    println!("write A: {a:?}");
    println!("write B: {b:?}");
    let disk = on_disk(&path, B_AT, BLOCK);
    println!("B's block on disk: {}", describe(&disk));
    assert_eq!(disk, vec![OLD; BLOCK], "B's data did reach the disk");
    expect_old_after_remount(&path);
    assert!(a.is_err());
    assert!(b.is_err(), "ACKNOWLEDGED BUT NOT ON DISK: write B");
}

fn trials() -> u32 {
    std::env::var("ATLAS_WB_TRIALS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

/// Unordered: B writes a failing block while other threads write good ones, as fast as they can.
/// Returns the rounds in which B was acknowledged.
fn race(name: &str, others: usize, open: impl Fn(&Path) -> Box<dyn BlockStore>) -> (u32, u32) {
    let path = old_file(name);
    let _faults = Faults;
    fail_blocks(&path, B_AT, BLOCK as u64);
    let rounds = trials();
    let mut acknowledged = 0;
    for _ in 0..rounds {
        // A fresh device (and open file) each round: the fixed code refuses a failed one.
        let d = open(&path);
        let d = d.as_ref();
        let done = AtomicBool::new(false);
        let start = Barrier::new(others + 1);
        let b = thread::scope(|s| {
            for t in 0..others {
                let (done, start) = (&done, &start);
                s.spawn(move || {
                    let at = A_AT + (t * 64 * BLOCK) as u64;
                    start.wait();
                    while !done.load(Ordering::SeqCst) {
                        if d.write_at(0, at, &[b'a'; 4 * BLOCK]).is_err() {
                            break;
                        }
                    }
                });
            }
            start.wait();
            let b = d.write_at(0, B_AT, &[b'b'; BLOCK]);
            done.store(true, Ordering::SeqCst);
            b
        });
        acknowledged += u32::from(b.is_ok());
    }
    heal();
    assert_eq!(on_disk(&path, B_AT, BLOCK), vec![OLD; BLOCK]);
    expect_old_after_remount(&path);
    (acknowledged, rounds)
}

#[test]
#[ignore = "needs deploy/native/writeback-error-lab.sh"]
fn file_device_race_acknowledges_no_lost_write() {
    let (acknowledged, rounds) = race("race-file.data", 4, |p| {
        Box::new(FileDevice::open(p).unwrap())
    });
    println!("file backend: {acknowledged} of {rounds} lost writes were acknowledged");
    assert_eq!(acknowledged, 0);
}

#[test]
#[ignore = "needs deploy/native/writeback-error-lab.sh"]
fn aligned_device_race_acknowledges_no_lost_write() {
    let hwm = dir("ATLAS_WB_DIR").join("race-aligned.hwm");
    let (acknowledged, rounds) = race("race-aligned.data", 4, |p| {
        let io = PreadIo::open(p).unwrap();
        Box::new(RawDevice::open(io, &hwm, FILE_LEN as u64).unwrap())
    });
    println!("aligned (O_DSYNC) backend: {acknowledged} of {rounds} lost writes were acknowledged");
    assert_eq!(acknowledged, 0);
}

fn record(index: u64) -> WalRecord<String> {
    WalRecord {
        term: 1,
        index,
        command: format!("record-{index}"),
    }
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[test]
#[ignore = "needs deploy/native/writeback-error-lab.sh"]
fn log_does_not_count_a_record_the_disk_never_got() {
    let root = dir("ATLAS_WB_DIR").join("log");
    let mut wal = Wal::open(&root).unwrap();
    for i in 1..=3 {
        wal.append(&record(i)).unwrap();
    }
    let path = root.join("metadata.wal");
    let _faults = Faults;
    // The next record goes into the same, already allocated, block.
    fail_blocks(&path, 0, BLOCK as u64);
    let append = wal.append(&record(4));
    let retry = wal.sync();
    let synced = wal.synced_index();
    heal();
    let disk = fs::metadata(&path).map(|m| m.len()).unwrap() as usize;
    let disk = on_disk(&path, 0, disk);
    println!("append of record 4: {append:?}");
    println!("sync again: {retry:?}; synced_index {synced}");
    println!("record 4 on disk: {}", contains(&disk, "record-4"));
    assert!(contains(&disk, "record-3") && !contains(&disk, "record-4"));
    assert!(append.is_err());
    assert!(retry.is_err(), "COUNTED AS DURABLE BUT NOT ON DISK");
    assert_eq!(synced, 3);
}

fn volume(id: &str) -> MetaCommand {
    MetaCommand::CreateVolume {
        id: id.into(),
        name: id.into(),
        size_bytes: 4096,
    }
}

#[test]
#[ignore = "needs deploy/native/writeback-error-lab.sh"]
fn raft_leader_does_not_commit_an_entry_the_disk_never_got() {
    let root = dir("ATLAS_WB_DIR").join("raft");
    let mut node = RaftNode::open(RaftConfig::new("a", vec![], &root)).unwrap();
    while !node.is_leader() {
        node.tick().unwrap();
    }
    node.propose(volume("kept-volume")).unwrap();
    let committed = node.commit_index();
    let path = root.join("wal").join("metadata.wal");
    let _faults = Faults;
    fail_blocks(&path, 0, BLOCK as u64);
    let propose = node.propose(volume("lost-volume"));
    let tick = node.tick();
    let applied = node.catalog().volumes.contains_key("lost-volume");
    let commit = node.commit_index();
    heal();
    let len = fs::metadata(&path).unwrap().len() as usize;
    let disk = on_disk(&path, 0, len);
    let stored = contains(&disk, "lost-volume");
    println!("propose: {propose:?}; next tick: {tick:?}");
    println!("commit index {committed} -> {commit}; applied: {applied}; on disk: {stored}");
    assert!(contains(&disk, "kept-volume") && !stored);
    assert!(propose.is_err());
    assert!(!applied, "COMMITTED BUT NOT ON DISK");
    assert_eq!(commit, committed);
    assert!(tick.is_err());
}

#[test]
#[ignore = "needs deploy/native/writeback-error-lab.sh"]
fn reopened_catalog_store_does_not_trust_a_failed_checkpoint_in_the_page_cache() {
    let path = dir("ATLAS_WB_DIR").join("store").join("catalog.redb");
    let store = CatalogStore::open(&path).unwrap();
    let mut catalog = Catalog::default();
    catalog.apply(1, 1, &volume("first")).unwrap();
    store.checkpoint(&mut catalog).unwrap();
    let _faults = Faults;
    fail_file(&path);
    catalog.apply(1, 2, &volume("second")).unwrap();
    let checkpoint = store.checkpoint(&mut catalog);
    let again = store.checkpoint(&mut catalog);
    heal();
    drop(catalog);
    drop(store);
    let applied = |label: &str| {
        let index = CatalogStore::open(&path)
            .and_then(|s| Ok(s.load()?.map(|c| c.applied_index)))
            .map_err(|e| e.to_string());
        println!("{label}: applied index {index:?}");
        index
    };
    println!("checkpoint: {checkpoint:?}");
    println!("checkpoint again: {again:?}");
    let reopened = applied("reopened");
    drop_cache(&path);
    let from_disk = applied("reopened after dropping the cache again");
    assert!(checkpoint.is_err() && again.is_err());
    assert_eq!(reopened, Ok(Some(1)), "TRUSTED THE PAGE CACHE");
    assert_eq!(from_disk, Ok(Some(1)));
}
