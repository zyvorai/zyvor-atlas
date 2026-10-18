// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Tiering cold extents to object storage ([`crate::object`]).
//!
//! An extent is cold once neither written (extents are never rewritten, so its creation time)
//! nor read for [`TierPolicy::cold_after`]. Tiering one reads and verifies it, uploads it whole
//! as `<object_prefix>extents/<id>`, and commits [`MetaCommand::TierExtent`], which frees its
//! replicas on the data nodes. Reads then fetch the object and verify it against the extent's
//! checksum; a later write to the same range installs a new extent on the data nodes as usual.

use std::{collections::BTreeSet, time::Duration};

use super::{now_ms, NativeEngine, NativeError};
use crate::{metadata::MetaCommand, raft::RaftError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierPolicy {
    /// How long an extent must go unwritten and unread before it is tiered.
    pub cold_after: Duration,
    /// Smaller extents stay on the data nodes (each object costs a request).
    pub min_bytes: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct TierStats {
    /// Cold extents found.
    pub candidates: u64,
    pub tiered: u64,
    /// Bytes uploaded.
    pub bytes: u64,
    /// Extents that changed or were removed while being tiered; left for the next pass.
    pub deferred: u64,
    /// Objects under this engine's prefix that no extent referenced, deleted.
    pub orphans_deleted: u64,
}

impl NativeEngine {
    fn object_key(&self, extent_id: &str) -> String {
        format!("{}extents/{extent_id}", self.cfg.object_prefix)
    }

    /// Extents cold under `policy`, coldest first.
    pub fn cold_extents(&self, policy: &TierPolicy) -> Result<Vec<String>, NativeError> {
        let now = now_ms();
        let cold_ms = policy.cold_after.as_millis() as u64;
        let reads = self
            .reads
            .lock()
            .map_err(|_| NativeError::Poisoned("reads"))?
            .clone();
        let mut cold: Vec<(u64, String)> = self.with_catalog(|c| {
            c.extents
                .values()
                .filter(|m| {
                    let e = &m.extent;
                    m.refs > 0
                        && !m.tombstoned
                        && e.object.is_none()
                        && !e.replicas.is_empty()
                        && e.payload_len >= policy.min_bytes
                })
                .filter_map(|m| {
                    let e = &m.extent;
                    let touched = e
                        .created_ms
                        .max(self.opened_ms)
                        .max(reads.get(&e.id).copied().unwrap_or(0));
                    (now.saturating_sub(touched) >= cold_ms).then(|| (touched, e.id.clone()))
                })
                .collect()
        })?;
        cold.sort();
        Ok(cold.into_iter().map(|(_, id)| id).collect())
    }

    /// Moves one extent to object storage. `Ok(None)` if it is already tiered, gone, or changed
    /// before the move could commit.
    pub fn tier_extent(&self, id: &str) -> Result<Option<u64>, NativeError> {
        let store = self
            .cfg
            .objects
            .as_ref()
            .ok_or_else(|| NativeError::Invalid("no object store is configured".into()))?;
        self.write_fence()?;
        // Repair and the orphan sweep take the same lock, so neither runs between the upload
        // and the commit.
        let _repair = self
            .repair_lock
            .lock()
            .map_err(|_| NativeError::Poisoned("repair"))?;
        let Some(ext) = self.with_catalog(|c| c.extents.get(id).map(|e| e.extent.clone()))? else {
            return Ok(None);
        };
        if ext.object.is_some() || ext.replicas.is_empty() {
            return Ok(None);
        }
        let data = self.read_placed(&ext)?;
        let key = self.object_key(id);
        store.put(&key, &data)?;
        let cmd = MetaCommand::TierExtent {
            extent_id: id.to_string(),
            key: key.clone(),
            replicas: ext.replicas.clone(),
        };
        match self.commit(cmd, None) {
            Ok(()) => {
                self.telemetry.extent_tiered();
                if let Ok(mut r) = self.reads.lock() {
                    r.remove(id);
                }
                Ok(Some(data.len() as u64))
            }
            Err(NativeError::Metadata(_) | NativeError::Raft(RaftError::Rejected(_))) => {
                let _ = store.delete(&key);
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Deletes objects under this engine's prefix that no extent references: uploads whose
    /// commit never happened, and deletes GC couldn't finish.
    pub fn sweep_objects(&self) -> Result<u64, NativeError> {
        let Some(store) = &self.cfg.objects else {
            return Ok(0);
        };
        self.write_fence()?;
        let _repair = self
            .repair_lock
            .lock()
            .map_err(|_| NativeError::Poisoned("repair"))?;
        let prefix = self.object_key("");
        let keys = store.list(&prefix)?;
        let live: BTreeSet<String> = self.with_catalog(|c| {
            c.extents
                .values()
                .filter_map(|e| e.extent.object.clone())
                .collect()
        })?;
        let mut deleted = 0;
        for key in keys.into_iter().filter(|k| !live.contains(k)) {
            store.delete(&key)?;
            deleted += 1;
        }
        Ok(deleted)
    }

    /// Sweeps orphans, then tiers every extent cold under `policy`, coldest first, calling
    /// `pace` with the bytes of each upload (it may sleep to cap the rate) and stopping early
    /// once `stop` returns true. Under Raft only the leader can tier.
    pub fn tier_once(
        &self,
        policy: &TierPolicy,
        mut pace: impl FnMut(u64),
        stop: impl Fn() -> bool,
    ) -> Result<TierStats, NativeError> {
        let mut st = TierStats {
            orphans_deleted: self.sweep_objects()?,
            ..Default::default()
        };
        let cold = self.cold_extents(policy)?;
        st.candidates = cold.len() as u64;
        for id in cold {
            if stop() {
                break;
            }
            match self.tier_extent(&id)? {
                Some(n) => {
                    st.tiered += 1;
                    st.bytes += n;
                    pace(n);
                }
                None => st.deferred += 1,
            }
        }
        if let Ok(mut r) = self.reads.lock() {
            let floor = now_ms().saturating_sub(policy.cold_after.as_millis() as u64);
            r.retain(|_, t| *t >= floor);
        }
        Ok(st)
    }
}
