// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! The mount's client session: a lease the cluster expires unless renewed, opened on first use
//! and renewed in the background. File locks are held by the session, so other mounts see them
//! and a mount that dies releases its locks when the lease runs out; `flock(2)` reaches the
//! cluster as a whole-file lock, as on NFS. With cache leases, a second thread polls the leader
//! for leases to give back and hands them to [`Recall`].

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use reqwest::Method;
use serde_json::json;

use crate::{
    client::{Body, Client, Error, Retry},
    ops::Errno,
};

// `c_short` on macOS, `c_int` on Linux; FUSE carries an `i32`.
#[allow(clippy::unnecessary_cast)]
pub const F_RDLCK: i32 = libc::F_RDLCK as i32;
#[allow(clippy::unnecessary_cast)]
pub const F_WRLCK: i32 = libc::F_WRLCK as i32;
#[allow(clippy::unnecessary_cast)]
pub const F_UNLCK: i32 = libc::F_UNLCK as i32;

/// Drops whatever the mount cached under the leases on these inodes.
pub type Recall = Arc<dyn Fn(&[u64]) + Send + Sync>;

/// How long one recall poll waits on the leader.
const RECALL_POLL_MS: u64 = 10_000;

/// A conflicting lock reported to `F_GETLK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict {
    pub start: u64,
    pub end: u64,
    pub typ: i32,
    pub pid: u32,
}

#[derive(Default)]
struct State {
    session: Option<String>,
    /// `(ino, owner)` pairs that took a lock through this session.
    held: HashSet<(u64, u64)>,
}

pub struct Locks {
    client: Arc<Client>,
    /// `/v1/fs/<fs>`.
    base: String,
    ttl: Duration,
    max_waiters: usize,
    waiters: AtomicUsize,
    state: Arc<Mutex<State>>,
    lost: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    renewer: Mutex<Option<JoinHandle<()>>>,
    recall: Option<Recall>,
}

fn kind(typ: i32) -> Result<Option<&'static str>, Errno> {
    match typ {
        F_RDLCK => Ok(Some("read")),
        F_WRLCK => Ok(Some("write")),
        F_UNLCK => Ok(None),
        _ => Err(libc::EINVAL),
    }
}

fn is(e: &Error, code: &str) -> bool {
    matches!(e, Error::Api { code: c, .. } if c == code)
}

impl Locks {
    /// `recall`: the session holds cache leases and gives them back through it.
    pub fn new(
        client: Arc<Client>,
        fs: &str,
        ttl: Duration,
        max_waiters: usize,
        recall: Option<Recall>,
    ) -> Self {
        Self {
            client,
            base: format!("/v1/fs/{fs}"),
            ttl,
            max_waiters,
            waiters: AtomicUsize::new(0),
            state: Arc::new(Mutex::new(State::default())),
            lost: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            renewer: Mutex::new(None),
            recall,
        }
    }

    /// Times this mount's session expired (or was closed) under it, losing its locks.
    pub fn sessions_lost(&self) -> usize {
        self.lost.load(Ordering::Relaxed)
    }

    /// The current session id, if one is open.
    pub fn session_id(&self) -> Option<String> {
        self.state.lock().ok().and_then(|s| s.session.clone())
    }

    /// The open session, opening one (and starting its renewal) if needed.
    pub fn session(&self) -> Result<String, Errno> {
        let mut st = self.state.lock().map_err(|_| libc::EIO)?;
        if let Some(s) = &st.session {
            return Ok(s.clone());
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.client
            .request(
                Method::POST,
                &format!("{}/sessions", self.base),
                Body::Json(json!({
                    "session": id,
                    "ttl_ms": self.ttl.as_millis() as u64,
                    "cache": self.recall.is_some(),
                })),
                Retry::Idempotent,
            )
            .map_err(|e| e.errno())?;
        st.session = Some(id.clone());
        drop(st);
        self.start_renewer();
        Ok(id)
    }

    fn start_renewer(&self) {
        let Ok(mut r) = self.renewer.lock() else {
            return;
        };
        if r.is_some() {
            return;
        }
        let (client, base, state, lost, stop) = (
            self.client.clone(),
            self.base.clone(),
            self.state.clone(),
            self.lost.clone(),
            self.stop.clone(),
        );
        if let Some(recall) = self.recall.clone() {
            self.start_recaller(recall);
        }
        let every = self.ttl / 3;
        *r = Some(thread::spawn(move || {
            let mut next = Instant::now() + every;
            while !stop.load(Ordering::SeqCst) {
                if Instant::now() < next {
                    thread::sleep(Duration::from_millis(50).min(every));
                    continue;
                }
                next = Instant::now() + every;
                let Some(id) = state.lock().ok().and_then(|s| s.session.clone()) else {
                    continue;
                };
                let renewed = client.request(
                    Method::POST,
                    &format!("{base}/sessions/{id}/renew"),
                    Body::Empty,
                    Retry::Idempotent,
                );
                match renewed {
                    Ok(_) => {}
                    Err(e) if is(&e, "no_session") => {
                        tracing::error!("lock session expired; its locks are lost");
                        lost.fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut s) = state.lock() {
                            if s.session.as_deref() == Some(id.as_str()) {
                                s.session = None;
                                s.held.clear();
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "lock session renewal failed"),
                }
            }
        }));
    }

    /// Polls the leader for cache leases to give back. Not joined at close: it notices the stop
    /// flag after its current poll.
    fn start_recaller(&self, recall: Recall) {
        let (client, base, state, stop) = (
            self.client.clone(),
            self.base.clone(),
            self.state.clone(),
            self.stop.clone(),
        );
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let Some(id) = state.lock().ok().and_then(|s| s.session.clone()) else {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                };
                let polled = client.json(
                    Method::GET,
                    &format!("{base}/sessions/{id}/recalls?wait_ms={RECALL_POLL_MS}"),
                    Body::Empty,
                    Retry::Idempotent,
                );
                let inos: Vec<u64> = match polled {
                    Ok(v) => serde_json::from_value(v["inos"].clone()).unwrap_or_default(),
                    Err(e) => {
                        tracing::debug!(error = %e, "cache lease recall poll failed");
                        thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                };
                if inos.is_empty() {
                    continue;
                }
                recall(&inos);
                if let Err(e) = client.request(
                    Method::POST,
                    &format!("{base}/sessions/{id}/recalls/done"),
                    Body::Json(json!({ "inos": inos })),
                    Retry::Idempotent,
                ) {
                    tracing::warn!(error = %e, "giving cache leases back failed; they expire");
                }
            }
        });
    }

    /// `F_GETLK`: the first lock another owner holds that would block `typ` over `[start, end]`.
    pub fn getlk(
        &self,
        ino: u64,
        owner: u64,
        start: u64,
        end: u64,
        typ: i32,
    ) -> Result<Option<Conflict>, Errno> {
        let Some(kind) = kind(typ)? else {
            return Ok(None);
        };
        // Without a session this mount holds nothing, so any overlapping lock conflicts.
        let session = self.session_id().unwrap_or_else(|| "-".into());
        let v = self
            .client
            .json(
                Method::GET,
                &format!(
                    "{}/inodes/{ino}/locks?session={session}&owner={owner}&kind={kind}\
                     &start={start}&end={end}",
                    self.base
                ),
                Body::Empty,
                Retry::Idempotent,
            )
            .map_err(|e| e.errno())?;
        let c = &v["conflict"];
        if c.is_null() {
            return Ok(None);
        }
        let typ = if c["kind"] == "write" {
            F_WRLCK
        } else {
            F_RDLCK
        };
        Ok(Some(Conflict {
            start: c["start"].as_u64().unwrap_or(0),
            end: c["end"].as_u64().unwrap_or(u64::MAX),
            typ,
            pid: c["pid"].as_u64().unwrap_or(0) as u32,
        }))
    }

    /// `F_SETLK` (or `F_SETLKW` with `wait`): EAGAIN on a conflict unless waiting, ENOLCK when
    /// every allowed waiter is already blocked.
    #[allow(clippy::too_many_arguments)]
    pub fn setlk(
        &self,
        ino: u64,
        owner: u64,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        wait: bool,
    ) -> Result<(), Errno> {
        let kind = kind(typ)?;
        if kind.is_none() && self.session_id().is_none() {
            return Ok(());
        }
        let mut waiting = false;
        let mut pause = Duration::from_millis(10);
        let result = loop {
            match self.try_setlk(ino, owner, start, end, kind, pid) {
                Err(libc::EAGAIN) if wait => {
                    if !waiting {
                        if self.waiters.fetch_add(1, Ordering::SeqCst) >= self.max_waiters {
                            self.waiters.fetch_sub(1, Ordering::SeqCst);
                            break Err(libc::ENOLCK);
                        }
                        waiting = true;
                    }
                    thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_millis(250));
                }
                r => break r,
            }
        };
        if waiting {
            self.waiters.fetch_sub(1, Ordering::SeqCst);
        }
        result
    }

    fn try_setlk(
        &self,
        ino: u64,
        owner: u64,
        start: u64,
        end: u64,
        kind: Option<&str>,
        pid: u32,
    ) -> Result<(), Errno> {
        let session = self.session()?;
        let sent = self.client.request(
            Method::POST,
            &format!("{}/inodes/{ino}/locks", self.base),
            Body::Json(json!({
                "session": session,
                "owner": owner,
                "kind": kind.unwrap_or("unlock"),
                "start": start,
                "end": end,
                "pid": pid,
            })),
            Retry::Idempotent,
        );
        match sent {
            Ok(_) => {
                if kind.is_some() {
                    if let Ok(mut s) = self.state.lock() {
                        s.held.insert((ino, owner));
                    }
                }
                Ok(())
            }
            Err(e) if is(&e, "locked") => Err(libc::EAGAIN),
            Err(e) if is(&e, "no_session") => {
                // The lease ran out: what it held is gone. A new lock starts a new session.
                self.lost.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut s) = self.state.lock() {
                    if s.session.as_deref() == Some(session.as_str()) {
                        s.session = None;
                        s.held.clear();
                    }
                }
                if kind.is_some() {
                    self.try_setlk(ino, owner, start, end, kind, pid)
                } else {
                    Ok(())
                }
            }
            Err(e) => Err(e.errno()),
        }
    }

    /// Releases the owner's locks on `ino` (the file was closed); free if it took none here.
    pub fn release(&self, ino: u64, owner: u64) -> Result<(), Errno> {
        let session = {
            let Ok(mut s) = self.state.lock() else {
                return Err(libc::EIO);
            };
            if !s.held.remove(&(ino, owner)) {
                return Ok(());
            }
            match &s.session {
                Some(id) => id.clone(),
                None => return Ok(()),
            }
        };
        match self.client.request(
            Method::POST,
            &format!("{}/inodes/{ino}/locks/release", self.base),
            Body::Json(json!({ "session": session, "owner": owner })),
            Retry::Idempotent,
        ) {
            Ok(_) => Ok(()),
            Err(e) if is(&e, "not_found") => Ok(()),
            Err(e) => Err(e.errno()),
        }
    }

    /// Closes the session (releasing every lock it holds) and stops renewing it.
    pub fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.renewer.lock().ok().and_then(|mut r| r.take()) {
            let _ = t.join();
        }
        let id = self.state.lock().ok().and_then(|mut s| {
            s.held.clear();
            s.session.take()
        });
        if let Some(id) = id {
            if let Err(e) = self.client.request(
                Method::DELETE,
                &format!("{}/sessions/{id}", self.base),
                Body::Empty,
                Retry::Idempotent,
            ) {
                tracing::warn!(error = %e, "closing the lock session failed; it expires on its own");
            }
        }
    }
}

impl Drop for Locks {
    fn drop(&mut self) {
        self.close();
    }
}
