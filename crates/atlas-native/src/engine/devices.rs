// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Which of a node's devices are offered writes, and which refuse them. A node's devices share
//! its failure domain, so a replica may go to any one of them: a device that fails is passed
//! over without giving up the node's others. How many replicas a write needs, and that each is
//! on a different node, does not change.

use std::{
    collections::BTreeSet,
    sync::{atomic::Ordering, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use super::{NativeEngine, NativeError, NodeRuntime};
use crate::device::BlockStore;

#[derive(Debug, Default)]
pub(super) struct DeviceHealth(Mutex<State>);

#[derive(Debug, Default)]
struct State {
    /// Not offered writes before this: a request to it failed.
    down_until: Option<Instant>,
    /// It reported that it refuses writes until its node restarts, and since when. Only its own
    /// later report clears this.
    failed_since: Option<Instant>,
}

impl DeviceHealth {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn offered(&self) -> bool {
        self.lock().down_until.is_none_or(|t| Instant::now() >= t)
    }
}

/// Whether `e` says that `device`'s node could not be reached, rather than that the device
/// failed a request the node answered. A local device has no node to lose.
pub(super) fn node_unreachable(device: &dyn BlockStore, e: &NativeError) -> bool {
    device.endpoint().is_some() && matches!(e, NativeError::Io(_))
}

impl NativeEngine {
    pub(super) fn has_offered_device(&self, node: &NodeRuntime) -> bool {
        node.device_health.iter().any(DeviceHealth::offered)
    }

    /// `node`'s device indexes in the order to try them for one write: round-robin.
    pub(super) fn device_order(&self, node: &NodeRuntime) -> impl Iterator<Item = usize> {
        let n = node.devices.len();
        let start = node.next_device.fetch_add(1, Ordering::Relaxed) % n;
        (0..n).map(move |k| (start + k) % n)
    }

    /// Whether to offer device `i` a write: it is not backed off, and if it refused writes
    /// before, it says that it no longer does. An error means its node could not be reached to
    /// ask.
    pub(super) fn device_usable(&self, node: &NodeRuntime, i: usize) -> Result<bool, NativeError> {
        let health = &node.device_health[i];
        if !health.offered() {
            return Ok(false);
        }
        if health.lock().failed_since.is_none() {
            return Ok(true);
        }
        match self.probe_device(node, i) {
            Ok(failed) => Ok(!failed),
            Err(e) if node_unreachable(node.devices[i].as_ref(), &e) => Err(e),
            Err(_) => Ok(false),
        }
    }

    /// The next device of `node` to offer a write, if any.
    pub(super) fn next_device(&self, node: &NodeRuntime) -> Option<usize> {
        self.device_order(node)
            .find(|&i| matches!(self.device_usable(node, i), Ok(true)))
    }

    fn back_off(&self, node: &NodeRuntime, i: usize) {
        node.device_health[i].lock().down_until = Some(Instant::now() + self.cfg.node_retry_after);
    }

    /// Asks device `i` whether it refuses writes until reopened, and records the answer.
    pub(super) fn probe_device(&self, node: &NodeRuntime, i: usize) -> Result<bool, NativeError> {
        match node.devices[i].probe() {
            Ok(Some(cause)) => {
                let newly = {
                    let mut state = node.device_health[i].lock();
                    state.down_until = Some(Instant::now() + self.cfg.node_retry_after);
                    let newly = state.failed_since.is_none();
                    state.failed_since.get_or_insert_with(Instant::now);
                    newly
                };
                if newly {
                    eprintln!(
                        "atlas-native: node {} device {i} refuses writes: {cause}",
                        node.spec.id
                    );
                }
                Ok(true)
            }
            Ok(None) => {
                node.device_health[i].lock().failed_since = None;
                Ok(false)
            }
            Err(e) => {
                // A node that cannot be reached is the node's state, not the device's.
                if !node_unreachable(node.devices[i].as_ref(), &e) {
                    self.back_off(node, i);
                }
                Err(e)
            }
        }
    }

    /// Probes each device of `node`. False if none answered, or the node could not be reached
    /// about its first one. (Not about a later one: a node too old to serve several devices
    /// closes the connection on being asked, and is otherwise fine.)
    pub(super) fn probe_node_devices(&self, node: &NodeRuntime) -> bool {
        let mut answered = false;
        for (i, device) in node.devices.iter().enumerate() {
            match self.probe_device(node, i) {
                Ok(_) => answered = true,
                Err(e) if i == 0 && node_unreachable(device.as_ref(), &e) => return false,
                Err(_) => {}
            }
        }
        answered
    }

    /// A write to device `i`, which its node answered, failed.
    pub(super) fn device_write_failed(&self, node: &NodeRuntime, i: usize) {
        self.back_off(node, i);
        // The answer is recorded; an unanswered question leaves the back-off.
        let _ = self.probe_device(node, i);
        if node.devices.len() == 1 {
            self.mark_down(node);
        } else {
            node.health.failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A read from device `i` failed. A node that cannot be reached, or whose only device this
    /// is, is backed off; one device failing leaves the node's others in use.
    pub(super) fn read_failed(&self, node: &NodeRuntime, i: usize, e: &NativeError) {
        if node.devices.len() == 1 || node_unreachable(node.devices[i].as_ref(), e) {
            self.mark_down(node);
        } else {
            node.health.failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A client writing directly reported that its writes to `node` failed. It was given the
    /// devices `leased`: if one of several now refuses writes, only that one is set aside.
    /// Otherwise the node is backed off.
    pub(super) fn client_write_failed(&self, node: &NodeRuntime, leased: &BTreeSet<usize>) {
        let mut refusing = false;
        // A node with one device is backed off whatever it would answer, so it is not asked;
        // and one that cannot be reached is asked once, as the commit waits for it.
        if node.devices.len() > 1 {
            for &i in leased.iter().filter(|i| **i < node.devices.len()) {
                match self.probe_device(node, i) {
                    Ok(failed) => refusing |= failed,
                    Err(e) if node_unreachable(node.devices[i].as_ref(), &e) => break,
                    Err(_) => {}
                }
            }
        }
        if refusing {
            node.health.failures.fetch_add(1, Ordering::Relaxed);
        } else {
            self.mark_down(node);
        }
    }

    pub(super) fn is_failed(&self, node: &NodeRuntime, i: usize) -> bool {
        node.device_health
            .get(i)
            .is_some_and(|d| d.lock().failed_since.is_some())
    }

    pub(super) fn known_failed(&self, node: &NodeRuntime) -> Vec<usize> {
        (0..node.devices.len())
            .filter(|&i| self.is_failed(node, i))
            .collect()
    }

    /// Asks every device of every node not backed off whether it refuses writes, in parallel
    /// across nodes.
    pub fn probe_devices(&self) {
        std::thread::scope(|s| {
            for n in self.nodes.iter().filter(|n| self.is_up(n)) {
                s.spawn(move || self.probe_node_devices(n));
            }
        });
    }

    /// Devices known to have refused writes for at least `after`, as `(node id, index)`.
    pub fn failed_devices(&self, after: Duration) -> BTreeSet<(String, usize)> {
        let now = Instant::now();
        self.nodes
            .iter()
            .flat_map(|n| {
                n.device_health
                    .iter()
                    .enumerate()
                    .filter(move |(_, d)| {
                        d.lock()
                            .failed_since
                            .is_some_and(|t| now.duration_since(t) >= after)
                    })
                    .map(move |(i, _)| (n.spec.id.clone(), i))
            })
            .collect()
    }
}
