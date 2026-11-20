// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Cache leases: a client session may cache an inode's attributes (and, for a directory, the
//! names in it) while it holds a lease on the inode. The leader keeps leases in memory. A change
//! to an inode first recalls every other session's lease on it and waits until each holder
//! acknowledges (having dropped its cache) or its lease runs out; leases are not granted on an
//! inode while a change to it is in flight. A new leader knows none of its predecessor's leases,
//! so while caching sessions are open it holds changes back for one lease period after taking
//! over.

use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        atomic::{AtomicBool, Ordering},
        Condvar, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use crate::engine::NativeError;

/// How long a granted lease lasts. Clients count it from when they sent the request.
pub const CACHE_LEASE: Duration = Duration::from_secs(5);

/// Longest a recall poll waits for something to recall.
pub const MAX_RECALL_WAIT: Duration = Duration::from_secs(25);

type Key = (String, u64);

#[derive(Default)]
struct State {
    /// The term this node leads in and when it was first seen leading it.
    term: u64,
    since: Option<Instant>,
    /// Holders of each inode's lease and when each runs out.
    leases: HashMap<Key, HashMap<String, Instant>>,
    /// Changes in flight per inode.
    changing: HashMap<Key, usize>,
    /// Leases each session has been asked to give back.
    recalls: HashMap<String, BTreeSet<Key>>,
    grants: u64,
}

#[derive(Default)]
pub struct CacheLeases {
    state: Mutex<State>,
    changed: Condvar,
}

/// A change in flight; dropping it lets leases on its inodes be granted again.
pub struct Change<'a> {
    leases: &'a CacheLeases,
    keys: Vec<Key>,
}

impl Drop for Change<'_> {
    fn drop(&mut self) {
        if let Ok(mut st) = self.leases.state.lock() {
            for k in &self.keys {
                if let Some(n) = st.changing.get_mut(k) {
                    *n -= 1;
                    if *n == 0 {
                        st.changing.remove(k);
                    }
                }
            }
        }
        self.leases.changed.notify_all();
    }
}

fn keys(fs: &str, inos: &[u64]) -> Vec<Key> {
    let set: BTreeSet<u64> = inos.iter().copied().collect();
    set.into_iter().map(|i| (fs.to_string(), i)).collect()
}

impl State {
    /// Forgets everything from an earlier term: those leases were granted by another leader (or
    /// by this node before it lost leadership), and the grace period covers them.
    fn sync(&mut self, term: u64) {
        if self.since.is_none() || self.term != term {
            *self = State {
                term,
                since: Some(Instant::now()),
                ..State::default()
            };
        }
    }

    fn sweep(&mut self) {
        let now = Instant::now();
        self.leases.retain(|_, h| {
            h.retain(|_, until| *until > now);
            !h.is_empty()
        });
    }

    /// Unexpired leases on `keys` held by sessions other than `except`.
    fn foreign(&self, keys: &[Key], except: Option<&str>) -> Vec<(String, Key)> {
        let now = Instant::now();
        keys.iter()
            .flat_map(|k| {
                self.leases.get(k).into_iter().flat_map(move |h| {
                    h.iter()
                        .filter(move |(s, until)| Some(s.as_str()) != except && **until > now)
                        .map(move |(s, _)| (s.clone(), k.clone()))
                })
            })
            .collect()
    }
}

impl CacheLeases {
    fn lock(&self) -> Result<MutexGuard<'_, State>, NativeError> {
        self.state
            .lock()
            .map_err(|_| NativeError::Poisoned("cache leases"))
    }

    /// Grants `session` a lease on each inode, unless a change to one is in flight. The caller
    /// confirms its leadership (a read barrier) first and reads what it returns after this.
    pub fn grant(
        &self,
        term: u64,
        fs: &str,
        inos: &[u64],
        session: &str,
    ) -> Result<Option<Duration>, NativeError> {
        let mut st = self.lock()?;
        st.sync(term);
        let keys = keys(fs, inos);
        if keys.iter().any(|k| st.changing.contains_key(k)) {
            return Ok(None);
        }
        st.grants += 1;
        if st.grants.is_multiple_of(1024) {
            st.sweep();
        }
        let until = Instant::now() + CACHE_LEASE;
        for k in keys {
            st.leases
                .entry(k)
                .or_default()
                .insert(session.into(), until);
        }
        Ok(Some(CACHE_LEASE))
    }

    /// Starts a change to `inos` by `session` (`None`: a client without one): waits out the
    /// grace period after a leader change if `grace` (caching sessions are open), then recalls
    /// every other session's lease on them and waits until each is given back or runs out.
    pub fn begin(
        &self,
        term: u64,
        grace: bool,
        fs: &str,
        inos: &[u64],
        session: Option<&str>,
    ) -> Result<Change<'_>, NativeError> {
        let keys = keys(fs, inos);
        let mut st = self.lock()?;
        st.sync(term);
        if grace {
            let ready = st.since.map_or_else(Instant::now, |s| s + CACHE_LEASE);
            while Instant::now() < ready {
                st = self
                    .changed
                    .wait_timeout(st, ready - Instant::now())
                    .map_err(|_| NativeError::Poisoned("cache leases"))?
                    .0;
            }
        }
        for k in &keys {
            *st.changing.entry(k.clone()).or_default() += 1;
        }
        let change = Change {
            leases: self,
            keys: keys.clone(),
        };
        let held = st.foreign(&keys, session);
        if held.is_empty() {
            return Ok(change);
        }
        for (s, k) in held {
            st.recalls.entry(s).or_default().insert(k);
        }
        self.changed.notify_all();
        loop {
            let now = Instant::now();
            let Some(until) = keys
                .iter()
                .filter_map(|k| st.leases.get(k))
                .flat_map(|h| {
                    h.iter()
                        .filter(|(s, _)| Some(s.as_str()) != session)
                        .map(|(_, u)| *u)
                })
                .filter(|u| *u > now)
                .max()
            else {
                break;
            };
            st = self
                .changed
                .wait_timeout(st, until - now)
                .map_err(|_| NativeError::Poisoned("cache leases"))?
                .0;
        }
        Ok(change)
    }

    /// Inodes of `fs` that `session` is asked to give back, waiting up to `wait` for one (less
    /// once `stop` is set).
    pub fn recalls(
        &self,
        fs: &str,
        session: &str,
        wait: Duration,
        stop: &AtomicBool,
    ) -> Result<Vec<u64>, NativeError> {
        let deadline = Instant::now() + wait.min(MAX_RECALL_WAIT);
        let mut st = self.lock()?;
        loop {
            let inos: Vec<u64> = st
                .recalls
                .get(session)
                .into_iter()
                .flatten()
                .filter(|(f, _)| f == fs)
                .map(|(_, i)| *i)
                .collect();
            let now = Instant::now();
            if !inos.is_empty() || now >= deadline || stop.load(Ordering::SeqCst) {
                return Ok(inos);
            }
            st = self
                .changed
                .wait_timeout(st, (deadline - now).min(Duration::from_millis(200)))
                .map_err(|_| NativeError::Poisoned("cache leases"))?
                .0;
        }
    }

    /// `session` dropped its cache of these inodes: their leases end.
    pub fn give_back(&self, fs: &str, session: &str, inos: &[u64]) -> Result<(), NativeError> {
        let mut st = self.lock()?;
        for k in keys(fs, inos) {
            if let Some(h) = st.leases.get_mut(&k) {
                h.remove(session);
                if h.is_empty() {
                    st.leases.remove(&k);
                }
            }
            if let Some(r) = st.recalls.get_mut(session) {
                r.remove(&k);
                if r.is_empty() {
                    st.recalls.remove(session);
                }
            }
        }
        drop(st);
        self.changed.notify_all();
        Ok(())
    }

    /// A closed session's leases all end.
    pub fn forget_session(&self, session: &str) -> Result<(), NativeError> {
        let mut st = self.lock()?;
        st.recalls.remove(session);
        st.leases.retain(|_, h| {
            h.remove(session);
            !h.is_empty()
        });
        drop(st);
        self.changed.notify_all();
        Ok(())
    }

    /// Leases currently held (for metrics).
    pub fn held(&self) -> usize {
        self.state.lock().map_or(0, |st| {
            let now = Instant::now();
            st.leases
                .values()
                .map(|h| h.values().filter(|u| **u > now).count())
                .sum()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};

    #[test]
    fn a_change_waits_for_the_holder_to_give_its_lease_back() {
        let l = Arc::new(CacheLeases::default());
        assert_eq!(l.grant(1, "f", &[5], "a").unwrap(), Some(CACHE_LEASE));
        // The holder's own changes do not recall its lease.
        drop(l.begin(1, false, "f", &[5], Some("a")).unwrap());
        let started = Instant::now();
        let holder = {
            let l = l.clone();
            thread::spawn(move || {
                let inos = l
                    .recalls("f", "a", Duration::from_secs(5), &AtomicBool::new(false))
                    .unwrap();
                assert_eq!(inos, vec![5]);
                thread::sleep(Duration::from_millis(100));
                l.give_back("f", "a", &inos).unwrap();
            })
        };
        let change = l.begin(1, false, "f", &[5, 6], Some("b")).unwrap();
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(100), "{waited:?}");
        assert!(waited < CACHE_LEASE, "{waited:?}");
        // No lease is granted while the change is in flight.
        assert_eq!(l.grant(1, "f", &[6], "a").unwrap(), None);
        drop(change);
        assert!(l.grant(1, "f", &[6], "a").unwrap().is_some());
        holder.join().unwrap();
        assert!(l
            .recalls("f", "a", Duration::ZERO, &AtomicBool::new(false))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_new_term_forgets_leases_and_closed_sessions_lose_theirs() {
        let l = CacheLeases::default();
        l.grant(1, "f", &[1], "a").unwrap();
        assert_eq!(l.held(), 1);
        // A change in a new term does not wait for leases from the old one (grace covers them).
        drop(l.begin(2, false, "f", &[1], None).unwrap());
        assert_eq!(l.held(), 0);
        l.grant(2, "f", &[1, 2], "a").unwrap();
        l.forget_session("a").unwrap();
        assert_eq!(l.held(), 0);
    }
}
