// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Which metadata group serves an id. An object lives in one group: a new volume or filesystem
//! goes to the group its id hashes to, and a snapshot or clone joins its source's group (they
//! share its extents). Ids are therefore looked up, not hashed: in this replica's catalogs first,
//! then, on a miss, again after a read barrier on every group, since this replica may not have
//! applied an object another node just created. Raising the group count later is safe: existing
//! objects are still found where they are and only new ids hash differently.

use super::{MetaGroup, NodeShared};
use crate::{engine::ObjectKind, metadata::MetaError, NativeError};

/// The group a new object named `id` goes to when nothing places it elsewhere (FNV-1a).
pub(super) fn home(id: &str, groups: usize) -> usize {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in id.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    (h % groups.max(1) as u64) as usize
}

impl NodeShared {
    fn find(&self, kind: ObjectKind, id: &str) -> Result<Option<usize>, NativeError> {
        for (i, g) in self.groups.iter().enumerate() {
            if g.engine.holds(kind, id)? {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }

    /// The group holding `id`, if any group does.
    pub(super) fn locate(&self, kind: ObjectKind, id: &str) -> Result<Option<usize>, NativeError> {
        if let Some(i) = self.find(kind, id)? {
            return Ok(Some(i));
        }
        if self.groups.len() == 1 {
            return Ok(None);
        }
        self.barrier_all()?;
        self.find(kind, id)
    }

    /// Catches every group's replica here up to its leader (see `NativeEngine::read_barrier`).
    pub(super) fn barrier_all(&self) -> Result<(), NativeError> {
        std::thread::scope(|s| {
            let barriers: Vec<_> = self
                .groups
                .iter()
                .map(|g| s.spawn(|| g.engine.read_barrier()))
                .collect();
            barriers.into_iter().try_for_each(|b| {
                b.join()
                    .unwrap_or(Err(NativeError::Poisoned("read barrier")))
            })
        })
    }

    /// The group holding `id`, else the one a new object named `id` goes to.
    pub(super) fn route(
        &self,
        kind: ObjectKind,
        id: &str,
    ) -> Result<(usize, &MetaGroup), NativeError> {
        let i = match self.locate(kind, id)? {
            Some(i) => i,
            None => home(id, self.groups.len()),
        };
        Ok((i, &self.groups[i]))
    }

    /// Refuses to create `id` in `group` when another group already holds it.
    pub(super) fn claim(
        &self,
        kind: ObjectKind,
        id: &str,
        group: usize,
    ) -> Result<(), NativeError> {
        match self.locate(kind, id)? {
            Some(g) if g != group => {
                Err(MetaError::Exists(format!("{id} (held by metadata group {g})")).into())
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::home;

    #[test]
    fn ids_spread_over_the_groups_and_stay_put() {
        let mut seen = [0usize; 4];
        for i in 0..400 {
            seen[home(&format!("vol-{i}"), 4)] += 1;
        }
        assert!(seen.iter().all(|n| *n > 60), "{seen:?}");
        assert_eq!(home("vol-7", 4), home("vol-7", 4));
        assert_eq!(home("anything", 1), 0);
    }
}
