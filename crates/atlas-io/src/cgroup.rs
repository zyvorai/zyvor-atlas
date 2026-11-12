// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Userspace cgroup → volume attribution. The collector consults this when a device map has no
//! volume for an event. With the `bpf` feature the same binding is mirrored into the `atlas_cgroup`
//! hash (64-byte volume id, matching `bpf/atlas_cgroup.bpf.c`) so `bpftool` can read it while the
//! agent is attached. The userspace map is what attribution uses; the kernel map is not required
//! for the HTTP API.

use std::collections::BTreeMap;

use serde::Serialize;

/// Longest volume id stored, excluding the trailing NUL the BPF value keeps.
pub const VOLUME_ID_MAX: usize = 63;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CgroupBinding {
    pub cgroup_id: u64,
    pub volume_id: String,
}

#[derive(Debug, Default, Clone)]
pub struct CgroupMap {
    by_id: BTreeMap<u64, String>,
}

impl CgroupMap {
    pub fn bind(&mut self, cgroup_id: u64, volume_id: String) -> Result<(), String> {
        validate(cgroup_id, &volume_id)?;
        self.by_id.insert(cgroup_id, volume_id);
        Ok(())
    }

    pub fn unbind(&mut self, cgroup_id: u64) -> bool {
        self.by_id.remove(&cgroup_id).is_some()
    }

    pub fn volume(&self, cgroup_id: u64) -> Option<String> {
        self.by_id.get(&cgroup_id).cloned()
    }

    pub fn list(&self) -> Vec<CgroupBinding> {
        self.by_id
            .iter()
            .map(|(id, volume)| CgroupBinding {
                cgroup_id: *id,
                volume_id: volume.clone(),
            })
            .collect()
    }
}

pub fn validate(cgroup_id: u64, volume_id: &str) -> Result<(), String> {
    if cgroup_id == 0 {
        return Err("cgroup_id must be non-zero".into());
    }
    if volume_id.is_empty() || volume_id.len() > VOLUME_ID_MAX {
        return Err(format!(
            "volume_id must be 1 to {VOLUME_ID_MAX} characters"
        ));
    }
    if !volume_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
    {
        return Err(
            "volume_id may contain only letters, digits, '_', '-', '.' and ':'".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_list_and_reject() {
        let mut m = CgroupMap::default();
        m.bind(42, "vol_cg".into()).unwrap();
        assert_eq!(m.volume(42).as_deref(), Some("vol_cg"));
        assert_eq!(m.list().len(), 1);
        assert!(m.bind(0, "vol_cg".into()).is_err());
        assert!(m.bind(7, "has space".into()).is_err());
        assert!(m.bind(7, "x".repeat(64)).is_err());
        assert!(m.unbind(42));
        assert!(m.volume(42).is_none());
        assert!(!m.unbind(42));
    }
}
