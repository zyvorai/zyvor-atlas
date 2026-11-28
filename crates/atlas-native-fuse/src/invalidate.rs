// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! A thread that drops the kernel's cached copy of files whose buffered writes were lost. The
//! kernel still holds the bytes it was told were written, and would serve them to readers.
//!
//! The invalidation runs off the request threads because it can wait for pages that a request
//! in progress holds locked. A reply that reports a loss goes through the same thread, after
//! the invalidation, so a caller that has seen the error cannot then read the lost bytes.

use std::{
    collections::HashSet,
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
};

type Reply = Box<dyn FnOnce() + Send>;

enum Job {
    Drop(u64),
    DropThen(u64, Reply),
}

/// The sending side; the thread ends once every clone is gone.
#[derive(Clone)]
pub struct Invalidator {
    jobs: Sender<Job>,
    /// Files with a [`Job::Drop`] waiting, so one file is queued once.
    queued: Arc<Mutex<HashSet<u64>>>,
}

/// The receiving side, to be served once there is a kernel to notify.
pub struct Queue {
    jobs: Receiver<Job>,
    queued: Arc<Mutex<HashSet<u64>>>,
}

pub fn channel() -> (Invalidator, Queue) {
    let (tx, rx) = mpsc::channel();
    let queued = Arc::new(Mutex::new(HashSet::new()));
    (
        Invalidator {
            jobs: tx,
            queued: queued.clone(),
        },
        Queue { jobs: rx, queued },
    )
}

impl Invalidator {
    /// Drops the cached copy of `ino` soon. Never blocks.
    pub fn lost(&self, ino: u64) {
        if self.queued.lock().is_ok_and(|mut q| q.insert(ino)) {
            let _ = self.jobs.send(Job::Drop(ino));
        }
    }

    /// Drops the cached copy of `ino`, then calls `reply`. Never blocks; without the thread,
    /// `reply` is called at once.
    pub fn then(&self, ino: u64, reply: impl FnOnce() + Send + 'static) {
        if let Err(mpsc::SendError(Job::DropThen(_, reply))) =
            self.jobs.send(Job::DropThen(ino, Box::new(reply)))
        {
            reply();
        }
    }
}

impl Queue {
    /// Starts the thread; `drop_cached` drops the kernel's copy of one inode.
    pub fn serve(
        self,
        drop_cached: impl Fn(u64) + Send + 'static,
    ) -> std::io::Result<JoinHandle<()>> {
        thread::Builder::new()
            .name("invalidate".into())
            .spawn(move || {
                for job in self.jobs {
                    match job {
                        Job::Drop(ino) => {
                            // Removed first: a loss during the call queues another.
                            if let Ok(mut q) = self.queued.lock() {
                                q.remove(&ino);
                            }
                            drop_cached(ino);
                        }
                        Job::DropThen(ino, reply) => {
                            drop_cached(ino);
                            reply();
                        }
                    }
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_queued_once_and_a_reply_follows_its_invalidation() {
        let (inv, queue) = channel();
        let log = Arc::new(Mutex::new(Vec::new()));
        inv.lost(1);
        inv.lost(1);
        inv.lost(2);
        {
            let log = log.clone();
            inv.then(1, move || log.lock().unwrap().push("reply".to_string()));
        }
        let thread = {
            let log = log.clone();
            queue
                .serve(move |ino| log.lock().unwrap().push(format!("drop {ino}")))
                .unwrap()
        };
        drop(inv);
        thread.join().unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            ["drop 1", "drop 2", "drop 1", "reply"]
        );
    }

    #[test]
    fn a_reply_is_not_lost_without_the_thread() {
        let (inv, queue) = channel();
        drop(queue);
        let (tx, rx) = mpsc::channel();
        inv.then(1, move || tx.send(()).unwrap());
        rx.try_recv().unwrap();
    }
}
