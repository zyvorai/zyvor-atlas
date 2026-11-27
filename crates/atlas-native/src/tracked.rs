// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! A `BTreeMap` that remembers which keys changed since the last checkpoint, so the catalog
//! store writes only those records. Reads go through `Deref`; every mutation goes through a
//! method here, which is what makes the record complete.

use std::{
    borrow::Borrow,
    collections::{BTreeMap, BTreeSet},
    ops::Deref,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone)]
pub struct Tracked<K: Ord, V> {
    map: BTreeMap<K, V>,
    /// Keys whose value may have changed, been inserted or been removed.
    touched: BTreeSet<K>,
    /// Keys whose whole value was replaced or removed (not just edited in place).
    replaced: BTreeSet<K>,
}

impl<K: Ord, V> Default for Tracked<K, V> {
    fn default() -> Self {
        Self {
            map: BTreeMap::new(),
            touched: BTreeSet::new(),
            replaced: BTreeSet::new(),
        }
    }
}

impl<K: Ord + Clone, V> Tracked<K, V> {
    pub fn get_mut<Q>(&mut self, k: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Ord + ToOwned<Owned = K> + ?Sized,
    {
        let v = self.map.get_mut(k)?;
        self.touched.insert(k.to_owned());
        Some(v)
    }

    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        self.touched.insert(k.clone());
        self.replaced.insert(k.clone());
        self.map.insert(k, v)
    }

    pub fn remove<Q>(&mut self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Ord + ToOwned<Owned = K> + ?Sized,
    {
        let v = self.map.remove(k)?;
        self.touched.insert(k.to_owned());
        self.replaced.insert(k.to_owned());
        Some(v)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.touched.extend(self.map.keys().cloned());
        self.map.values_mut()
    }

    /// Adds a value the store already holds as it is (an inode paged in), recording no change.
    pub(crate) fn insert_quietly(&mut self, k: K, v: V) {
        self.map.insert(k, v);
    }

    /// Every value, for edits that aren't part of the record (attaching a cache).
    pub(crate) fn values_mut_quietly(&mut self) -> impl Iterator<Item = &mut V> {
        self.map.values_mut()
    }

    pub fn into_values(self) -> impl Iterator<Item = V> {
        self.map.into_values()
    }

    /// Keys changed since [`Self::clear_changes`].
    pub fn touched(&self) -> &BTreeSet<K> {
        &self.touched
    }

    /// Keys inserted or removed (rather than edited in place) since [`Self::clear_changes`].
    pub fn replaced(&self) -> &BTreeSet<K> {
        &self.replaced
    }

    pub fn clear_changes(&mut self) {
        self.touched.clear();
        self.replaced.clear();
    }

    /// [`Self::clear_changes`], first calling `f` on each changed value still present, to clear
    /// maps nested in it (a nested map only changes through its parent's `get_mut`).
    pub fn clear_changes_with(&mut self, mut f: impl FnMut(&mut V)) {
        for k in &self.touched {
            if let Some(v) = self.map.get_mut(k) {
                f(v);
            }
        }
        self.clear_changes();
    }
}

impl<K: Ord, V> Deref for Tracked<K, V> {
    type Target = BTreeMap<K, V>;
    fn deref(&self) -> &BTreeMap<K, V> {
        &self.map
    }
}

impl<K: Ord, V> From<BTreeMap<K, V>> for Tracked<K, V> {
    fn from(map: BTreeMap<K, V>) -> Self {
        Self {
            map,
            touched: BTreeSet::new(),
            replaced: BTreeSet::new(),
        }
    }
}

impl<K: Ord, V> FromIterator<(K, V)> for Tracked<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        BTreeMap::from_iter(iter).into()
    }
}

/// Equality is over contents; pending changes are bookkeeping.
impl<K: Ord, V: PartialEq> PartialEq for Tracked<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map
    }
}
impl<K: Ord, V: Eq> Eq for Tracked<K, V> {}

impl<K: Ord + Serialize, V: Serialize> Serialize for Tracked<K, V> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.map.serialize(s)
    }
}

impl<'de, K: Ord + Deserialize<'de>, V: Deserialize<'de>> Deserialize<'de> for Tracked<K, V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        BTreeMap::deserialize(d).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_edits_inserts_and_removals() {
        let mut t: Tracked<u32, &str> = [(1, "a"), (2, "b")].into_iter().collect();
        assert!(t.touched().is_empty());
        *t.get_mut(&1).unwrap() = "A";
        t.insert(3, "c");
        t.remove(&2);
        assert!(t.get_mut(&9).is_none());
        assert_eq!(t.touched().iter().copied().collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(t.replaced().iter().copied().collect::<Vec<_>>(), [2, 3]);
        t.clear_changes();
        assert!(t.touched().is_empty() && t.replaced().is_empty());
        assert_eq!(serde_json::to_string(&t).unwrap(), r#"{"1":"A","3":"c"}"#);
    }
}
