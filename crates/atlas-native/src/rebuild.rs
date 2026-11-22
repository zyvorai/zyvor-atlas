// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! The rebuild controller a metadata leader runs for each group it leads.
//!
//! Each [`Rebuilder::step`] first probes the data nodes, then works on one batch:
//!
//! - **Rebuild:** extents with parts on nodes that have failed every request for
//!   [`RebuildConfig::delay`] are found from metadata alone and rebuilt onto other nodes, those
//!   with the least spare redundancy first, without waiting on reads from the lost nodes.
//! - **Scrub:** with nothing to rebuild, the next batch of extents (in id order, resuming where
//!   the last batch stopped) is read back and verified, so bit rot and silently lost parts are
//!   found; one pass over every extent starts each [`RebuildConfig::scrub_interval`].
//!
//! Rebuild and scrub traffic are paced separately (bytes read plus written per second), so
//! neither saturates the data nodes ahead of client I/O.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use crate::engine::{NativeEngine, NativeError, RepairStats};

/// Extents handled per step, after which the degraded list is recomputed.
const BATCH: usize = 64;
/// How often nodes are probed.
const PROBE_EVERY: Duration = Duration::from_secs(5);
/// How long an extent whose rebuild made no progress (no eligible target, or every part
/// unreadable) is left alone before it is tried again.
const RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildConfig {
    /// How long a node must fail every request before its parts are rebuilt elsewhere. A node
    /// back within this window (a restart, a brief partition) costs no rebuild.
    pub delay: Duration,
    /// Cap on rebuild traffic in bytes per second; 0 is unlimited.
    pub rebuild_bytes_per_sec: u64,
    /// Time between the starts of two scrub passes; zero disables scrubbing.
    pub scrub_interval: Duration,
    /// Cap on scrub traffic in bytes per second; 0 is unlimited.
    pub scrub_bytes_per_sec: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RebuildStatus {
    /// Nodes treated as lost: configured unhealthy, or failing for at least the delay.
    pub lost_nodes: Vec<String>,
    /// Extents with parts on lost nodes.
    pub degraded_extents: u64,
    /// Degraded extents that one more lost part would make unreadable.
    pub at_risk_extents: u64,
    /// Degraded extents with too few parts left to rebuild until a node returns.
    pub unrecoverable_extents: u64,
    /// Totals of the rebuilds this controller ran.
    pub rebuilt: RepairStats,
    pub scrub: ScrubStatus,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ScrubStatus {
    pub passes: u64,
    /// Progress of the pass under way.
    pub current: RepairStats,
    /// The last completed pass.
    pub last: Option<RepairStats>,
    /// Totals over every pass.
    pub total: RepairStats,
}

/// Spaces out work so that, on average, at most `rate` bytes go by per second.
#[derive(Debug)]
pub(crate) struct Pacer {
    rate: u64,
    ready_at: Instant,
}

impl Pacer {
    pub(crate) fn new(rate: u64) -> Self {
        Self {
            rate,
            ready_at: Instant::now(),
        }
    }

    /// Accounts for `bytes` just moved and sleeps until the budget allows more (or `stop`).
    pub(crate) fn pace(&mut self, bytes: u64, stop: &AtomicBool) {
        if self.rate == 0 || bytes == 0 {
            return;
        }
        let start = self.ready_at.max(Instant::now());
        self.ready_at = start + Duration::from_secs_f64(bytes as f64 / self.rate as f64);
        while !stop.load(Ordering::SeqCst) {
            let left = self.ready_at.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
    }
}

#[derive(Debug)]
pub struct Rebuilder {
    cfg: RebuildConfig,
    rebuild_pacer: Pacer,
    scrub_pacer: Pacer,
    /// Last extent id the scrub pass under way has checked.
    cursor: Option<String>,
    pass_started: Instant,
    next_pass: Instant,
    last_probe: Option<Instant>,
    /// Extents whose last rebuild made no progress, and when to try them again.
    stuck: BTreeMap<String, Instant>,
    status: RebuildStatus,
}

impl Rebuilder {
    /// The first scrub pass starts one interval from now.
    pub fn new(cfg: RebuildConfig) -> Self {
        let now = Instant::now();
        Self {
            cfg,
            rebuild_pacer: Pacer::new(cfg.rebuild_bytes_per_sec),
            scrub_pacer: Pacer::new(cfg.scrub_bytes_per_sec),
            cursor: None,
            pass_started: now,
            next_pass: now + cfg.scrub_interval,
            last_probe: None,
            stuck: BTreeMap::new(),
            status: RebuildStatus::default(),
        }
    }

    pub fn status(&self) -> &RebuildStatus {
        &self.status
    }

    /// Runs one batch of rebuild or scrub work on `e`. Returns whether more work is waiting,
    /// in which case the caller should step again without sleeping.
    pub fn step(&mut self, e: &NativeEngine, stop: &AtomicBool) -> Result<bool, NativeError> {
        let now = Instant::now();
        if self
            .last_probe
            .is_none_or(|t| now.duration_since(t) >= PROBE_EVERY)
        {
            e.probe_nodes();
            self.last_probe = Some(now);
        }
        let lost = e.lost_nodes(self.cfg.delay);
        let degraded = e.degraded_extents(&lost)?;
        self.stuck.retain(|_, until| *until > now);
        self.status.lost_nodes = lost.iter().cloned().collect();
        self.status.degraded_extents = degraded.len() as u64;
        self.status.at_risk_extents = degraded.iter().filter(|d| d.spare == 0).count() as u64;
        self.status.unrecoverable_extents = degraded.iter().filter(|d| d.spare < 0).count() as u64;

        let work: Vec<&str> = degraded
            .iter()
            .filter(|d| d.spare >= 0 && !self.stuck.contains_key(&d.extent_id))
            .take(BATCH)
            .map(|d| d.extent_id.as_str())
            .collect();
        if !work.is_empty() {
            for id in work {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let st = e.repair_extent(id, &lost)?;
                if st.replicas_repaired == 0 {
                    self.stuck.insert(id.to_string(), now + RETRY_AFTER);
                }
                self.status.rebuilt.add(&st);
                self.rebuild_pacer
                    .pace(st.bytes_read + st.bytes_written, stop);
            }
            return Ok(true);
        }

        if self.cfg.scrub_interval.is_zero() || (self.cursor.is_none() && now < self.next_pass) {
            return Ok(false);
        }
        if self.cursor.is_none() {
            self.pass_started = now;
        }
        let ids = e.extent_ids_after(self.cursor.as_deref(), BATCH)?;
        if ids.is_empty() {
            let pass = std::mem::take(&mut self.status.scrub.current);
            self.status.scrub.passes += 1;
            self.status.scrub.last = Some(pass);
            self.cursor = None;
            self.next_pass = self.pass_started + self.cfg.scrub_interval;
            return Ok(false);
        }
        for id in ids {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let st = e.repair_extent(&id, &lost)?;
            self.status.scrub.current.add(&st);
            self.status.scrub.total.add(&st);
            self.scrub_pacer
                .pace(st.bytes_read + st.bytes_written, stop);
            self.cursor = Some(id);
        }
        Ok(true)
    }

    /// The last completed scrub pass, if any.
    pub fn last_pass(&self) -> Option<RepairStats> {
        self.status.scrub.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pacer_spaces_work_to_its_rate() {
        let stop = AtomicBool::new(false);
        let mut p = Pacer::new(1 << 20);
        let t = Instant::now();
        for _ in 0..4 {
            p.pace(64 << 10, &stop);
        }
        let took = t.elapsed();
        assert!(took >= Duration::from_millis(240), "{took:?}");
        assert!(took < Duration::from_secs(2), "{took:?}");
    }

    #[test]
    fn an_unlimited_pacer_never_sleeps() {
        let stop = AtomicBool::new(false);
        let mut p = Pacer::new(0);
        let t = Instant::now();
        p.pace(1 << 40, &stop);
        assert!(t.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn stopping_interrupts_the_pacer() {
        let stop = AtomicBool::new(true);
        let mut p = Pacer::new(1);
        let t = Instant::now();
        p.pace(1 << 20, &stop);
        assert!(t.elapsed() < Duration::from_millis(50));
    }
}
