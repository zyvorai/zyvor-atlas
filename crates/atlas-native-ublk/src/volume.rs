// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Block requests against one atlas-native volume. Nothing here knows about the kernel.
//!
//! **Durability.** There is no write cache: a write completes only after the metadata leader
//! answered 2xx, which it does once the data nodes have stored the extents and the Raft group has
//! committed them. So every completed write is durable, a flush has nothing to do, and the device
//! declares no volatile cache.
//!
//! **Read-your-writes.** A node runs a read barrier before answering a data read, so any node
//! that answers includes every write completed before the request; one that cannot reach a
//! leader answers 503 and the next node is asked. Reads never pass `stale=1`. Nodes from before
//! the data routes ran a barrier answer from whatever they have applied: this client must not be
//! used with them.
//!
//! **Late writes.** The volume API has no write sequence, so a write the client gave up on (no
//! answer) may still be executing and could land after a newer write to the same blocks. Such a
//! request is not completed, successfully or not, until [`VolumeConfig::settle`] has passed. That
//! narrows the window; only the server could close it.

use std::{
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

use atlas_native_fuse::client::{Attempt, Body, Client, Error};
use reqwest::{Method, StatusCode};
use serde_json::Value;

use crate::{split, Errno};

const MAX_PAUSE: Duration = Duration::from_secs(1);
/// How long [`Volume::open`] looks for the volume when requests otherwise retry for ever.
const OPEN_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct VolumeConfig {
    /// Logical block size of the device: 512, 1024, 2048 or 4096.
    pub block_bytes: u32,
    /// Largest cluster request; at most the nodes' `max_request_bytes`.
    pub max_request_bytes: u64,
    /// How long a request keeps retrying before it fails with `EIO`; `None` never gives up.
    pub io_timeout: Option<Duration>,
    /// How long a write is held after a request of it went unanswered.
    pub settle: Duration,
    pub read_only: bool,
}

impl Default for VolumeConfig {
    fn default() -> Self {
        Self {
            block_bytes: 4096,
            max_request_bytes: 8 << 20,
            io_timeout: Some(Duration::from_secs(60)),
            settle: Duration::from_secs(15),
            read_only: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// The volume's size, rounded down to whole blocks.
    pub size_bytes: u64,
    pub block_bytes: u32,
    /// The cluster's extent grid.
    pub extent_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
    Flush,
    Discard,
    WriteZeroes,
}

#[derive(Debug)]
pub struct Request {
    pub op: Op,
    pub offset: u64,
    pub len: u64,
    /// The bytes of a write; empty otherwise.
    pub data: Vec<u8>,
}

/// The bytes of a read (empty for other operations), or the errno the request fails with.
pub type Reply = Result<Vec<u8>, Errno>;

#[derive(Debug)]
enum Failure {
    Retry(String),
    Fail(Errno, String),
}

/// Why a request failed: the errno to report and the last answer, for people.
#[derive(Debug)]
struct GaveUp {
    errno: Errno,
    why: String,
}

impl GaveUp {
    fn aborted() -> Self {
        Self {
            errno: libc::EIO,
            why: "aborted".into(),
        }
    }
}

/// Whether an error answer is worth asking again, and the errno if not.
fn refused(status: StatusCode, error: &Error) -> Failure {
    let code = match error {
        Error::Api { code, .. } => code.as_str(),
        _ => "",
    };
    let msg = error.to_string();
    match status.as_u16() {
        // Not the leader, no quorum or too few replicas, and "internal", which is what a node
        // that is shutting down or lost a data node mid-request answers.
        408 | 421 | 429 | 500 | 502 | 503 | 504 => Failure::Retry(msg),
        409 if code == "busy" => Failure::Retry(msg),
        409 if code == "read_only" => Failure::Fail(libc::EROFS, msg),
        409 if code == "quota" => Failure::Fail(libc::ENOSPC, msg),
        400 => Failure::Fail(libc::EINVAL, msg),
        _ => Failure::Fail(libc::EIO, msg),
    }
}

/// Set once, to make every waiting and future request fail instead of retrying.
#[derive(Default)]
struct Abort {
    set: Mutex<bool>,
    changed: Condvar,
}

impl Abort {
    fn set(&self) {
        *self.set.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.changed.notify_all();
    }

    /// Sleeps for `d`; true if aborted (before or during the sleep).
    fn wait(&self, d: Duration) -> bool {
        let g = self.set.lock().unwrap_or_else(|e| e.into_inner());
        let (g, _) = self
            .changed
            .wait_timeout_while(g, d, |set| !*set)
            .unwrap_or_else(|e| e.into_inner());
        *g
    }
}

pub struct Volume {
    client: Client,
    id: String,
    cfg: VolumeConfig,
    geometry: Geometry,
    abort: Abort,
}

impl Volume {
    /// Looks volume `id` up and checks that the cluster accepts requests of the configured size.
    pub fn open(client: Client, id: &str, cfg: VolumeConfig) -> Result<Self, String> {
        let block = u64::from(cfg.block_bytes);
        if !(512..=4096).contains(&block) || !block.is_power_of_two() {
            return Err(format!("block size {block} is not 512, 1024, 2048 or 4096"));
        }
        if cfg.max_request_bytes < block || !cfg.max_request_bytes.is_multiple_of(block) {
            return Err(format!(
                "max request size {} is not a multiple of the block size {block}",
                cfg.max_request_bytes
            ));
        }
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(format!("volume id {id:?} is not [A-Za-z0-9-]+"));
        }
        let mut volume = Self {
            client,
            id: id.into(),
            cfg,
            geometry: Geometry {
                size_bytes: 0,
                block_bytes: 0,
                extent_bytes: 0,
            },
            abort: Abort::default(),
        };
        let deadline = Some(Instant::now() + volume.cfg.io_timeout.unwrap_or(OPEN_TIMEOUT));
        volume.geometry = volume.discover(deadline)?;
        // Reads and writes share the node's limit, so one largest read shows whether it holds.
        let len = volume.cfg.max_request_bytes.min(volume.geometry.size_bytes) as usize;
        volume.read_piece(0, len, deadline).map_err(|e| {
            format!(
                "reading {len} bytes of volume {id} failed ({}); \
                 --max-request-bytes must not exceed the nodes' max_request_bytes",
                e.why
            )
        })?;
        Ok(volume)
    }

    fn discover(&self, deadline: Option<Instant>) -> Result<Geometry, String> {
        // The volume list has no read barrier, so first make the node catch up with an empty
        // read, then list on that same node.
        let barrier = format!("/v1/volumes/{}/data?offset=0&len=0", self.id);
        let mut node = self.client.preferred();
        let (volumes, status) = self
            .until(
                self.client.endpoint_count(),
                deadline,
                "opening the volume",
                || {
                    let found = self.get(node, &barrier).and_then(|_| {
                        Ok((
                            self.get_json(node, "/v1/volumes")?,
                            self.get_json(node, "/v1/status")?,
                        ))
                    });
                    match &found {
                        Ok(_) => self.client.prefer(node),
                        Err(_) => node += 1,
                    }
                    found
                },
            )
            .map_err(|e| format!("volume {}: {}", self.id, e.why))?;
        let found = volumes["volumes"]
            .as_array()
            .and_then(|v| v.iter().find(|v| v["id"] == self.id.as_str()))
            .ok_or_else(|| format!("volume {} not found", self.id))?;
        let size = found["size_bytes"]
            .as_u64()
            .ok_or("the volume list has no size_bytes")?;
        let extent_bytes = status["layout"]["extent_bytes"]
            .as_u64()
            .filter(|e| *e > 0)
            .ok_or("the node reports no extent size; point --endpoint at metadata nodes")?;
        let block = u64::from(self.cfg.block_bytes);
        let size_bytes = size - size % block;
        if size_bytes == 0 {
            return Err(format!("volume {} is smaller than one block", self.id));
        }
        if size_bytes != size {
            tracing::warn!(
                "volume {} is {size} bytes; the device ends at {size_bytes}",
                self.id
            );
        }
        Ok(Geometry {
            size_bytes,
            block_bytes: self.cfg.block_bytes,
            extent_bytes,
        })
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn read_only(&self) -> bool {
        self.cfg.read_only
    }

    /// The cluster client, for its endpoint preference.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Fails every request in progress and every later one with `EIO`. For giving up on a stop
    /// that waits on an unreachable cluster.
    pub fn abort(&self) {
        self.abort.set();
    }

    pub fn execute(&self, r: Request) -> Reply {
        match r.op {
            Op::Read => self.read(r.offset, r.len),
            Op::Write if r.data.len() as u64 != r.len => Err(libc::EINVAL),
            Op::Write => self.write(r.offset, &r.data).map(|()| Vec::new()),
            Op::Flush => self.flush().map(|()| Vec::new()),
            Op::WriteZeroes => self.write_zeroes(r.offset, r.len).map(|()| Vec::new()),
            // The volume API cannot release blocks.
            Op::Discard => Err(libc::EOPNOTSUPP),
        }
    }

    fn check(&self, offset: u64, len: u64) -> Result<(), Errno> {
        let block = u64::from(self.geometry.block_bytes);
        let inside = offset
            .checked_add(len)
            .is_some_and(|end| end <= self.geometry.size_bytes);
        if len == 0 || !offset.is_multiple_of(block) || !len.is_multiple_of(block) || !inside {
            return Err(libc::EINVAL);
        }
        Ok(())
    }

    pub fn read(&self, offset: u64, len: u64) -> Reply {
        self.check(offset, len)?;
        let mut out = Vec::with_capacity(len as usize);
        let deadline = self.deadline();
        for (at, n) in split::read_pieces(offset, len, self.cfg.max_request_bytes) {
            let mut piece = self.read_piece(at, n, deadline).map_err(|e| e.errno)?;
            out.append(&mut piece);
        }
        Ok(out)
    }

    /// Returns once all of `data` is durable. On an error any part of it may or may not have
    /// been written.
    pub fn write(&self, offset: u64, data: &[u8]) -> Result<(), Errno> {
        if self.cfg.read_only {
            return Err(libc::EROFS);
        }
        self.check(offset, data.len() as u64)?;
        let deadline = self.deadline();
        for (at, n) in split::write_pieces(
            offset,
            data.len() as u64,
            self.geometry.extent_bytes,
            self.cfg.max_request_bytes,
        ) {
            let from = (at - offset) as usize;
            self.write_piece(at, &data[from..from + n], deadline)
                .map_err(|e| e.errno)?;
        }
        Ok(())
    }

    /// Writes zeros. The blocks stay allocated: the cluster has no way to punch a hole.
    pub fn write_zeroes(&self, offset: u64, len: u64) -> Result<(), Errno> {
        if self.cfg.read_only {
            return Err(libc::EROFS);
        }
        self.check(offset, len)?;
        let deadline = self.deadline();
        let mut zeros = Vec::new();
        for (at, n) in split::write_pieces(
            offset,
            len,
            self.geometry.extent_bytes,
            self.cfg.max_request_bytes,
        ) {
            zeros.resize(n, 0);
            self.write_piece(at, &zeros, deadline)
                .map_err(|e| e.errno)?;
        }
        Ok(())
    }

    /// Every completed write is already durable, so there is nothing to wait for.
    pub fn flush(&self) -> Result<(), Errno> {
        Ok(())
    }

    fn deadline(&self) -> Option<Instant> {
        self.cfg.io_timeout.map(|t| Instant::now() + t)
    }

    /// Calls `once` until it succeeds, fails for good, the deadline passes or [`Self::abort`] is
    /// called, pausing after every `per_round` failures.
    fn until<T>(
        &self,
        per_round: usize,
        deadline: Option<Instant>,
        what: &str,
        mut once: impl FnMut() -> Result<T, Failure>,
    ) -> Result<T, GaveUp> {
        let mut pause = Duration::from_millis(10);
        let mut failures = 0usize;
        loop {
            if self.abort.wait(Duration::ZERO) {
                return Err(GaveUp::aborted());
            }
            let why = match once() {
                Ok(v) => return Ok(v),
                Err(Failure::Fail(errno, why)) => {
                    tracing::error!("{what} failed: {why}");
                    return Err(GaveUp { errno, why });
                }
                Err(Failure::Retry(why)) => why,
            };
            if deadline.is_some_and(|d| Instant::now() >= d) {
                tracing::error!("{what} timed out: {why}");
                return Err(GaveUp {
                    errno: libc::EIO,
                    why,
                });
            }
            tracing::debug!("{what}, retrying: {why}");
            failures += 1;
            if failures.is_multiple_of(per_round.max(1)) {
                if self.abort.wait(pause) {
                    return Err(GaveUp::aborted());
                }
                if pause == MAX_PAUSE {
                    tracing::warn!("{what} still failing: {why}");
                }
                pause = (pause * 2).min(MAX_PAUSE);
            }
        }
    }

    fn get(&self, node: usize, path: &str) -> Result<Vec<u8>, Failure> {
        match self.client.attempt(node, &Method::GET, path, &Body::Empty) {
            Attempt::Done(b) => Ok(b),
            Attempt::Refused { status, error } => Err(refused(status, &error)),
            Attempt::NoAnswer { error, .. } => Err(Failure::Retry(error.to_string())),
        }
    }

    fn get_json(&self, node: usize, path: &str) -> Result<Value, Failure> {
        serde_json::from_slice(&self.get(node, path)?)
            .map_err(|e| Failure::Fail(libc::EIO, format!("{path}: {e}")))
    }

    fn read_piece(
        &self,
        offset: u64,
        len: usize,
        deadline: Option<Instant>,
    ) -> Result<Vec<u8>, GaveUp> {
        let path = format!("/v1/volumes/{}/data?offset={offset}&len={len}", self.id);
        let what = format!("read of {len} bytes at {offset}");
        let mut node = self.client.preferred();
        self.until(self.client.endpoint_count(), deadline, &what, || {
            let got = self.get(node, &path).and_then(|b| match b.len() {
                n if n == len => Ok(b),
                n => Err(Failure::Retry(format!("node returned {n} of {len} bytes"))),
            });
            match &got {
                Ok(_) => self.client.prefer(node),
                Err(_) => node += 1,
            }
            got
        })
    }

    fn write_piece(
        &self,
        offset: u64,
        data: &[u8],
        deadline: Option<Instant>,
    ) -> Result<(), GaveUp> {
        let path = format!("/v1/volumes/{}/data?offset={offset}", self.id);
        let what = format!("write of {} bytes at {offset}", data.len());
        let mut node = self.client.preferred();
        // When a request of this write was last given up on while it might still be running.
        let mut unanswered = None;
        let result = self.until(self.client.endpoint_count(), deadline, &what, || {
            let failure = match self
                .client
                .attempt(node, &Method::PUT, &path, &Body::Bytes(data))
            {
                Attempt::Done(_) => {
                    self.client.prefer(node);
                    return Ok(());
                }
                Attempt::Refused { status, error } => refused(status, &error),
                Attempt::NoAnswer {
                    may_still_run,
                    error,
                } => {
                    if may_still_run {
                        unanswered = Some(Instant::now());
                    }
                    Failure::Retry(error.to_string())
                }
            };
            node += 1;
            Err(failure)
        });
        if let Some(at) = unanswered {
            let left = self.cfg.settle.saturating_sub(at.elapsed());
            if !left.is_zero() {
                tracing::warn!("{what}: a request went unanswered; holding it for {left:?}");
                if self.abort.wait(left) {
                    return Err(GaveUp::aborted());
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errno_of(status: u16, code: &str) -> Option<Errno> {
        let e = Error::Api {
            status,
            code: code.into(),
            message: String::new(),
        };
        match refused(StatusCode::from_u16(status).unwrap(), &e) {
            Failure::Retry(_) => None,
            Failure::Fail(errno, _) => Some(errno),
        }
    }

    #[test]
    fn error_answers_are_retried_or_mapped() {
        for (status, code) in [
            (421, "not_leader"),
            (503, "unavailable"),
            (500, "internal"),
            (409, "busy"),
        ] {
            assert_eq!(errno_of(status, code), None, "{status} {code}");
        }
        assert_eq!(errno_of(404, "not_found"), Some(libc::EIO));
        assert_eq!(errno_of(401, ""), Some(libc::EIO));
        assert_eq!(errno_of(413, ""), Some(libc::EIO));
        assert_eq!(errno_of(400, "invalid"), Some(libc::EINVAL));
        assert_eq!(errno_of(409, "read_only"), Some(libc::EROFS));
        assert_eq!(errno_of(409, "quota"), Some(libc::ENOSPC));
        assert_eq!(errno_of(409, "invalid"), Some(libc::EIO));
    }
}
