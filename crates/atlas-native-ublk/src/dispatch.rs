// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! A fixed pool of threads that run block requests, so the kernel-facing threads never block on
//! the network. Every submitted request gets exactly one completion, also if running it panics.

use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{mpsc, Arc, Mutex},
    thread::{self, JoinHandle},
};

use crate::volume::{Reply, Request};

pub type Execute = Arc<dyn Fn(Request) -> Reply + Send + Sync>;
type Done = Box<dyn FnOnce(Reply) + Send>;

pub struct Dispatcher {
    jobs: Mutex<Option<mpsc::Sender<(Request, Done)>>>,
    workers: Vec<JoinHandle<()>>,
}

impl Dispatcher {
    /// `workers` requests run at once; more wait in line. A ublk tag has one request at a time,
    /// so one worker per tag never makes a request wait.
    pub fn new(workers: usize, execute: Execute) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel::<(Request, Done)>();
        let rx = Arc::new(Mutex::new(rx));
        let workers = (0..workers.max(1))
            .map(|i| {
                let (rx, execute) = (rx.clone(), execute.clone());
                thread::Builder::new()
                    .name(format!("ublk-io-{i}"))
                    .spawn(move || loop {
                        let job = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                        let Ok((request, done)) = job else { return };
                        let reply = catch_unwind(AssertUnwindSafe(|| execute(request)))
                            .unwrap_or(Err(libc::EIO));
                        done(reply);
                    })
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        Ok(Self {
            jobs: Mutex::new(Some(tx)),
            workers,
        })
    }

    /// Runs `request` on a worker, which then calls `done`.
    pub fn submit(&self, request: Request, done: impl FnOnce(Reply) + Send + 'static) {
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let refused = match jobs.as_ref() {
            Some(tx) => tx.send((request, Box::new(done) as Done)).err(),
            None => return done(Err(libc::EIO)),
        };
        if let Some(mpsc::SendError((_, done))) = refused {
            done(Err(libc::EIO));
        }
    }
}

impl Drop for Dispatcher {
    /// Runs what is queued, then stops the workers.
    fn drop(&mut self) {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner()).take();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::Op;
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Barrier,
        },
        time::Duration,
    };

    fn request(offset: u64) -> Request {
        Request {
            op: Op::Read,
            offset,
            len: 512,
            data: Vec::new(),
        }
    }

    #[test]
    fn requests_run_at_once_and_each_completes_once() {
        // Four requests that can only finish if all four run concurrently.
        let together = Arc::new(Barrier::new(4));
        let pool = Dispatcher::new(4, {
            let together = together.clone();
            Arc::new(move |r: Request| {
                together.wait();
                Ok(r.offset.to_le_bytes().to_vec())
            })
        })
        .unwrap();
        let (tx, rx) = mpsc::channel();
        for i in 0..4u64 {
            let tx = tx.clone();
            pool.submit(request(i), move |r| tx.send((i, r)).unwrap());
        }
        let mut seen: Vec<u64> = (0..4)
            .map(|_| {
                let (i, r) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
                assert_eq!(r.unwrap(), i.to_le_bytes());
                i
            })
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, [0, 1, 2, 3]);
    }

    #[test]
    fn a_panic_fails_the_request_and_the_worker_survives() {
        let done = Arc::new(AtomicUsize::new(0));
        let pool = Dispatcher::new(
            1,
            Arc::new(|r: Request| {
                assert!(r.offset != 0, "boom");
                Ok(Vec::new())
            }),
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        for i in 0..3u64 {
            let (tx, done) = (tx.clone(), done.clone());
            pool.submit(request(i), move |r| {
                done.fetch_add(1, Ordering::SeqCst);
                tx.send(r).unwrap();
            });
        }
        assert_eq!(rx.recv().unwrap(), Err(libc::EIO));
        assert_eq!(rx.recv().unwrap(), Ok(Vec::new()));
        assert_eq!(rx.recv().unwrap(), Ok(Vec::new()));
        drop(pool);
        assert_eq!(done.load(Ordering::SeqCst), 3);
    }
}
