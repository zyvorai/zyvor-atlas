// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Snapshot exports to and imports from object storage, run one at a time on a worker thread.
//! A transfer runs on the leader of the group holding its snapshot (export) or volume (import)
//! and fails if this node stops leading that group; the client resubmits it to the new leader.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{atomic::Ordering, Condvar, Mutex},
    time::Duration,
};

use serde::Serialize;
use serde_json::{json, Value};

use super::NodeShared;
use crate::{engine::NativeError, raft::Role};

/// Finished transfers kept for `GET /v1/transfers`.
const KEEP_FINISHED: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum State {
    Queued,
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct Transfer {
    pub id: String,
    /// `export` or `import`.
    pub kind: &'static str,
    pub export: String,
    /// The exported snapshot, or the volume imported into.
    pub source: String,
    pub state: State,
    /// Extents transferred of `total`.
    pub done: u64,
    pub total: u64,
    pub error: Option<String>,
    /// Export: [`crate::ExportStats`]; import: `{"bytes": n}`.
    pub result: Option<Value>,
}

#[derive(Debug)]
struct Job {
    id: String,
    group: usize,
}

#[derive(Debug, Default)]
pub(super) struct Transfers {
    next: Mutex<u64>,
    all: Mutex<BTreeMap<String, Transfer>>,
    queue: Mutex<VecDeque<Job>>,
    wake: Condvar,
}

impl Transfers {
    /// Queues an export of `snapshot` (in `group`) as `export`; returns the transfer id.
    pub fn export(&self, group: usize, snapshot: &str, export: &str) -> String {
        self.push("export", group, export, snapshot)
    }

    /// Queues an import of `export` into `volume` (in `group`); returns the transfer id.
    pub fn import(&self, group: usize, export: &str, volume: &str) -> String {
        self.push("import", group, export, volume)
    }

    fn push(&self, kind: &'static str, group: usize, export: &str, source: &str) -> String {
        let id = {
            let mut n = self.next.lock().unwrap_or_else(|e| e.into_inner());
            *n += 1;
            format!("t{n}")
        };
        let t = Transfer {
            id: id.clone(),
            kind,
            export: export.to_string(),
            source: source.to_string(),
            state: State::Queued,
            done: 0,
            total: 0,
            error: None,
            result: None,
        };
        {
            let mut all = self.all.lock().unwrap_or_else(|e| e.into_inner());
            all.insert(id.clone(), t);
            let finished: Vec<String> = all
                .values()
                .filter(|t| matches!(t.state, State::Done | State::Failed))
                .map(|t| t.id.clone())
                .collect();
            for old in finished
                .iter()
                .take(finished.len().saturating_sub(KEEP_FINISHED))
            {
                all.remove(old);
            }
        }
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(Job {
                id: id.clone(),
                group,
            });
        self.wake.notify_one();
        id
    }

    pub fn get(&self, id: &str) -> Option<Transfer> {
        self.all
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    /// Every transfer, oldest first.
    pub fn list(&self) -> Vec<Transfer> {
        let mut v: Vec<Transfer> = self
            .all
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        v.sort_by_key(|t| t.id[1..].parse::<u64>().unwrap_or(0));
        v
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut Transfer)) {
        if let Some(t) = self
            .all
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(id)
        {
            f(t);
        }
    }
}

/// The worker: runs queued transfers until the node stops.
pub(super) fn run(sh: &NodeShared) {
    let tr = &sh.transfers;
    while !sh.stop.load(Ordering::SeqCst) {
        let job = {
            let q = tr.queue.lock().unwrap_or_else(|e| e.into_inner());
            let (mut q, _) = tr
                .wake
                .wait_timeout_while(q, Duration::from_millis(100), |q| q.is_empty())
                .unwrap_or_else(|e| e.into_inner());
            q.pop_front()
        };
        let Some(job) = job else { continue };
        let Some(t) = tr.get(&job.id) else { continue };
        tr.update(&job.id, |t| t.state = State::Running);
        let result = execute(sh, &job, &t);
        tr.update(&job.id, |t| match result {
            Ok(v) => {
                t.state = State::Done;
                t.result = Some(v);
            }
            Err(e) => {
                t.state = State::Failed;
                t.error = Some(e.to_string());
            }
        });
    }
}

fn execute(sh: &NodeShared, job: &Job, t: &Transfer) -> Result<Value, NativeError> {
    let group = &sh.groups[job.group];
    let progress = |done: u64, total: u64| {
        sh.transfers.update(&job.id, |t| {
            t.done = done;
            t.total = total;
        });
        !sh.stop.load(Ordering::SeqCst) && group.raft.status().is_ok_and(|s| s.role == Role::Leader)
    };
    match t.kind {
        "export" => group
            .engine
            .export_snapshot(&t.source, &t.export, progress)
            .map(|st| json!(st)),
        _ => group
            .engine
            .import_export(&t.export, &t.source, progress)
            .map(|bytes| json!({ "bytes": bytes })),
    }
}
