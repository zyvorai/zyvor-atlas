// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Background sending of full write-back runs, so a large sequential write (a checkpoint) keeps
//! filling the next run while earlier ones are on the wire, and several go at once.
//!
//! Runs of one inode that overlap are sent in order: a run waits until every overlapping run
//! ahead of it is done. A failed run's errno is kept for its inode and returned by the next
//! write to it and by [`Pipeline::wait`], which `flush`, `fsync` and `close` call, as with
//! NFS's asynchronous writes.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};

use crate::{ops::Errno, writeback::Dirty};

pub type SendRun = Arc<dyn Fn(&Dirty) -> Result<(), Errno> + Send + Sync>;

#[derive(Default)]
struct State {
    queue: VecDeque<(u64, Dirty)>,
    /// Runs queued or being sent, per inode: `(run id, start, end)`.
    inflight: HashMap<u64, Vec<(u64, u64, u64)>>,
    pending: usize,
    errors: HashMap<u64, Errno>,
    next: u64,
    stop: bool,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    /// Runs queued or in flight at most (each up to the write-back size).
    capacity: usize,
}

pub struct Pipeline {
    inner: Arc<Inner>,
    workers: Vec<JoinHandle<()>>,
}

impl Pipeline {
    /// `parallel` runs are sent at once and as many more may wait queued.
    pub fn new(parallel: usize, send: SendRun) -> Self {
        let parallel = parallel.max(1);
        let inner = Arc::new(Inner {
            state: Mutex::default(),
            changed: Condvar::new(),
            capacity: parallel * 2,
        });
        let workers = (0..parallel)
            .map(|_| {
                let (inner, send) = (inner.clone(), send.clone());
                thread::spawn(move || work(&inner, &send))
            })
            .collect();
        Self { inner, workers }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>, Errno> {
        self.inner.state.lock().map_err(|_| libc::EIO)
    }

    /// Queues `run`, first waiting for room and for overlapping runs of its inode.
    pub fn submit(&self, run: Dirty) -> Result<(), Errno> {
        let (start, end) = (run.offset, run.end());
        let mut s = self.lock()?;
        s = self
            .inner
            .changed
            .wait_while(s, |s| {
                s.pending >= self.inner.capacity
                    || s.inflight
                        .get(&run.ino)
                        .is_some_and(|v| v.iter().any(|(_, a, b)| *a < end && start < *b))
            })
            .map_err(|_| libc::EIO)?;
        s.next += 1;
        let id = s.next;
        s.inflight
            .entry(run.ino)
            .or_default()
            .push((id, start, end));
        s.pending += 1;
        s.queue.push_back((id, run));
        self.inner.changed.notify_all();
        Ok(())
    }

    /// The errno of a failed run of `ino` not yet reported by [`Self::wait`].
    pub fn failed(&self, ino: u64) -> Option<Errno> {
        self.lock().ok()?.errors.get(&ino).copied()
    }

    /// End of the furthest run of `ino` not yet sent, which the file size must account for.
    pub fn end(&self, ino: u64) -> Option<u64> {
        let s = self.lock().ok()?;
        s.inflight.get(&ino)?.iter().map(|(_, _, end)| *end).max()
    }

    /// Waits until every run of `ino` is sent; returns (and clears) the first failure.
    pub fn wait(&self, ino: u64) -> Result<(), Errno> {
        let s = self.lock()?;
        let mut s = self
            .inner
            .changed
            .wait_while(s, |s| s.inflight.contains_key(&ino))
            .map_err(|_| libc::EIO)?;
        s.errors.remove(&ino).map_or(Ok(()), Err)
    }

    /// Waits until every run is sent; returns (and clears) a failure, if any.
    pub fn wait_all(&self) -> Result<(), Errno> {
        let s = self.lock()?;
        let mut s = self
            .inner
            .changed
            .wait_while(s, |s| s.pending > 0)
            .map_err(|_| libc::EIO)?;
        let first = s.errors.values().next().copied();
        s.errors.clear();
        first.map_or(Ok(()), Err)
    }
}

fn work(inner: &Inner, send: &SendRun) {
    loop {
        let (id, run) = {
            let Ok(s) = inner.state.lock() else { return };
            let Ok(mut s) = inner
                .changed
                .wait_while(s, |s| s.queue.is_empty() && !s.stop)
            else {
                return;
            };
            match s.queue.pop_front() {
                Some(job) => job,
                None => return,
            }
        };
        let result = send(&run);
        let Ok(mut s) = inner.state.lock() else {
            return;
        };
        if let Some(v) = s.inflight.get_mut(&run.ino) {
            v.retain(|(i, _, _)| *i != id);
            if v.is_empty() {
                s.inflight.remove(&run.ino);
            }
        }
        s.pending -= 1;
        if let Err(e) = result {
            s.errors.entry(run.ino).or_insert(e);
        }
        inner.changed.notify_all();
    }
}

impl Drop for Pipeline {
    /// Sends what is queued, then stops the workers.
    fn drop(&mut self) {
        if let Ok(mut s) = self.inner.state.lock() {
            s.stop = true;
        }
        self.inner.changed.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::atomic::AtomicUsize, sync::atomic::Ordering, time::Duration};

    use super::*;

    fn run(ino: u64, offset: u64, len: usize) -> Dirty {
        Dirty {
            ino,
            offset,
            data: vec![0; len],
        }
    }

    #[test]
    fn disjoint_runs_go_at_once_and_overlapping_ones_in_order() {
        let now = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let order = Arc::new(Mutex::new(Vec::new()));
        let send: SendRun = {
            let (now, max, order) = (now.clone(), max.clone(), order.clone());
            Arc::new(move |r: &Dirty| {
                let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(n, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(20));
                order.lock().unwrap().push((r.offset, r.data.len()));
                now.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            })
        };
        let p = Pipeline::new(4, send);
        for i in 0..4 {
            p.submit(run(1, i * 10, 10)).unwrap();
        }
        assert_eq!(p.end(1), Some(40));
        // Overlaps the first run, so it is sent after it.
        p.submit(run(1, 5, 2)).unwrap();
        p.wait(1).unwrap();
        assert_eq!(p.end(1), None);
        assert!(max.load(Ordering::SeqCst) > 1);
        let order = order.lock().unwrap();
        let first = order.iter().position(|r| *r == (0, 10)).unwrap();
        let rewrite = order.iter().position(|r| *r == (5, 2)).unwrap();
        assert!(first < rewrite, "{order:?}");
    }

    #[test]
    fn a_failure_is_reported_to_its_inode_once() {
        let send: SendRun = Arc::new(|r: &Dirty| {
            if r.ino == 2 {
                Err(libc::ENOSPC)
            } else {
                Ok(())
            }
        });
        let p = Pipeline::new(2, send);
        p.submit(run(1, 0, 4)).unwrap();
        p.submit(run(2, 0, 4)).unwrap();
        p.wait(1).unwrap();
        assert_eq!(p.wait(2), Err(libc::ENOSPC));
        assert_eq!(p.failed(2), None);
        p.wait(2).unwrap();
        p.submit(run(2, 4, 4)).unwrap();
        assert_eq!(p.wait_all(), Err(libc::ENOSPC));
        p.wait_all().unwrap();
    }
}
