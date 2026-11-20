// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

use std::{
    collections::HashMap,
    hash::Hash,
    time::{Duration, Instant},
};

/// A map whose entries expire `ttl` after they were stored, or at an explicit time
/// ([`TtlCache::put_until`]). A zero `ttl` caches nothing through [`TtlCache::put`].
pub struct TtlCache<K, V> {
    ttl: Duration,
    map: HashMap<K, (V, Instant)>,
}

impl<K: Hash + Eq, V: Clone> TtlCache<K, V> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            map: HashMap::new(),
        }
    }

    pub fn get(&mut self, k: &K) -> Option<V> {
        match self.map.get(k) {
            Some((v, until)) if Instant::now() < *until => Some(v.clone()),
            Some(_) => {
                self.map.remove(k);
                None
            }
            None => None,
        }
    }

    /// Stores `v` for the TTL; with a zero TTL only drops what was cached under `k`.
    pub fn put(&mut self, k: K, v: V) {
        if self.ttl.is_zero() {
            self.map.remove(&k);
            return;
        }
        self.put_until(k, v, Instant::now() + self.ttl);
    }

    /// Stores `v` until `until`.
    pub fn put_until(&mut self, k: K, v: V, until: Instant) {
        // Expired entries are only dropped on access; sweep before the map grows unbounded.
        if self.map.len() >= 65_536 {
            let now = Instant::now();
            self.map.retain(|_, (_, u)| *u > now);
        }
        self.map.insert(k, (v, until));
    }

    pub fn remove(&mut self, k: &K) -> Option<V> {
        self.map.remove(k).map(|(v, _)| v)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        self.map.retain(|k, (v, _)| keep(k, v));
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_expire() {
        let mut c = TtlCache::new(Duration::from_millis(30));
        c.put(1, "a");
        assert_eq!(c.get(&1), Some("a"));
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(c.get(&1), None);
        let mut off = TtlCache::new(Duration::ZERO);
        off.put(1, "a");
        assert_eq!(off.get(&1), None);
        // An explicit expiry outlives a zero TTL, until a plain put replaces it.
        off.put_until(1, "b", Instant::now() + Duration::from_secs(5));
        assert_eq!(off.get(&1), Some("b"));
        off.put(1, "c");
        assert_eq!(off.get(&1), None);
    }
}
