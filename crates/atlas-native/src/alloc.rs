// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Device-space free lists. Lives inside the metadata catalog so that every allocation and release
//! is a deterministic consequence of an applied `MetaCommand` and survives WAL replay and Raft
//! replication unchanged.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreeRange {
    pub node_id: String,
    pub device_index: usize,
    pub offset: u64,
    pub len: u64,
}

impl FreeRange {
    fn end(&self) -> u64 {
        self.offset + self.len
    }

    fn same_device(&self, node_id: &str, device_index: usize) -> bool {
        self.node_id == node_id && self.device_index == device_index
    }

    fn key(&self) -> (&str, usize, u64) {
        (&self.node_id, self.device_index, self.offset)
    }
}

/// Sorted by `(node_id, device_index, offset)`, non-overlapping, adjacent ranges coalesced.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FreeList {
    ranges: Vec<FreeRange>,
}

impl FreeList {
    pub fn ranges(&self) -> &[FreeRange] {
        &self.ranges
    }

    pub fn total_bytes(&self) -> u64 {
        self.ranges.iter().map(|r| r.len).sum()
    }

    /// First-fit lookup. Does not mutate: the allocation only takes effect when the command that
    /// installs data there is applied (see [`FreeList::reserve`]).
    pub fn find(&self, node_id: &str, device_index: usize, len: u64) -> Option<u64> {
        if len == 0 {
            return None;
        }
        self.ranges
            .iter()
            .find(|r| r.same_device(node_id, device_index) && r.len >= len)
            .map(|r| r.offset)
    }

    /// Removes `[offset, offset+len)` from the free list if it lies inside one free range.
    /// Returns false when the space was not free (e.g. it was freshly appended).
    pub fn reserve(&mut self, node_id: &str, device_index: usize, offset: u64, len: u64) -> bool {
        if len == 0 {
            return false;
        }
        let Some(pos) = self.ranges.iter().position(|r| {
            r.same_device(node_id, device_index) && r.offset <= offset && offset + len <= r.end()
        }) else {
            return false;
        };
        let r = self.ranges.remove(pos);
        let mut insert_at = pos;
        if offset > r.offset {
            self.ranges.insert(
                insert_at,
                FreeRange {
                    len: offset - r.offset,
                    ..r.clone()
                },
            );
            insert_at += 1;
        }
        if offset + len < r.end() {
            self.ranges.insert(
                insert_at,
                FreeRange {
                    offset: offset + len,
                    len: r.end() - (offset + len),
                    ..r
                },
            );
        }
        true
    }

    /// Returns `[offset, offset+len)` to the free list. Overlap with an already-free range is a
    /// double free and is rejected.
    pub fn release(
        &mut self,
        node_id: &str,
        device_index: usize,
        offset: u64,
        len: u64,
    ) -> Result<(), String> {
        if len == 0 {
            return Ok(());
        }
        let new = FreeRange {
            node_id: node_id.to_string(),
            device_index,
            offset,
            len,
        };
        let pos = self.ranges.partition_point(|r| r.key() < new.key());
        if pos > 0 {
            let prev = &self.ranges[pos - 1];
            if prev.same_device(node_id, device_index) && prev.end() > offset {
                return Err(format!(
                    "double free on {node_id}/{device_index} at {offset}+{len}"
                ));
            }
        }
        if let Some(next) = self.ranges.get(pos) {
            if next.same_device(node_id, device_index) && new.end() > next.offset {
                return Err(format!(
                    "double free on {node_id}/{device_index} at {offset}+{len}"
                ));
            }
        }
        self.ranges.insert(pos, new);
        if pos + 1 < self.ranges.len() {
            let next = self.ranges[pos + 1].clone();
            if next.same_device(node_id, device_index) && self.ranges[pos].end() == next.offset {
                self.ranges[pos].len += next.len;
                self.ranges.remove(pos + 1);
            }
        }
        if pos > 0 {
            let cur = self.ranges[pos].clone();
            let prev = &mut self.ranges[pos - 1];
            if prev.same_device(node_id, device_index) && prev.end() == cur.offset {
                prev.len += cur.len;
                self.ranges.remove(pos);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_coalesces_and_reserve_splits() {
        let mut f = FreeList::default();
        f.release("n1", 0, 0, 10).unwrap();
        f.release("n1", 0, 20, 10).unwrap();
        f.release("n1", 0, 10, 10).unwrap();
        assert_eq!(f.ranges().len(), 1);
        assert_eq!(f.total_bytes(), 30);

        assert_eq!(f.find("n1", 0, 5), Some(0));
        assert!(f.reserve("n1", 0, 10, 5));
        assert_eq!(f.ranges().len(), 2);
        assert_eq!(f.total_bytes(), 25);
        assert!(!f.reserve("n1", 0, 12, 1));
    }

    #[test]
    fn double_free_is_rejected() {
        let mut f = FreeList::default();
        f.release("n1", 0, 100, 50).unwrap();
        assert!(f.release("n1", 0, 120, 10).is_err());
        assert!(f.release("n1", 0, 90, 20).is_err());
        f.release("n2", 0, 100, 50).unwrap();
    }

    #[test]
    fn find_is_per_device() {
        let mut f = FreeList::default();
        f.release("n1", 0, 0, 8).unwrap();
        assert_eq!(f.find("n2", 0, 4), None);
        assert_eq!(f.find("n1", 1, 4), None);
        assert_eq!(f.find("n1", 0, 9), None);
    }
}
