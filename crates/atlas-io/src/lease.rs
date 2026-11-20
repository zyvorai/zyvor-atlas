// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Time-limited write-freeze leases. Missing/expired lease ⇒ fail open.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use atlas_api_types::IoLease;
use uuid::Uuid;

const MAX_TTL_SECS: u64 = 15 * 60;

#[derive(Debug, Default)]
pub struct LeaseTable {
    by_id: HashMap<String, IoLease>,
}

impl LeaseTable {
    pub fn grant(
        &mut self,
        device: String,
        volume_id: Option<String>,
        ttl_secs: u64,
        reason: String,
        now: Option<u64>,
    ) -> Result<IoLease, String> {
        let ttl = ttl_secs.clamp(1, MAX_TTL_SECS);
        let now = now.unwrap_or_else(unix_now);
        let lease = IoLease {
            id: format!("iol_{}", Uuid::new_v4().simple()),
            device,
            volume_id,
            action: "write_freeze".into(),
            expires_unix: now.saturating_add(ttl),
            reason,
        };
        self.by_id.insert(lease.id.clone(), lease.clone());
        Ok(lease)
    }

    pub fn active(&self, now: Option<u64>) -> Vec<IoLease> {
        let now = now.unwrap_or_else(unix_now);
        let mut v: Vec<_> = self
            .by_id
            .values()
            .filter(|l| l.expires_unix > now)
            .cloned()
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    pub fn is_frozen(&self, device: &str, now: Option<u64>) -> bool {
        let now = now.unwrap_or_else(unix_now);
        self.by_id
            .values()
            .any(|l| l.device == device && l.expires_unix > now)
    }

    pub fn expire_all(&mut self) {
        self.by_id.clear();
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_open_when_expired() {
        let mut t = LeaseTable::default();
        let l = t
            .grant("rbd0".into(), None, 10, "incident".into(), Some(1000))
            .unwrap();
        assert!(t.is_frozen("rbd0", Some(1005)));
        assert!(!t.is_frozen("rbd0", Some(l.expires_unix)));
        assert!(!t.is_frozen("rbd0", Some(l.expires_unix + 1)));
        assert!(!t.is_frozen("zd0", Some(1005)));
    }

    #[test]
    fn ttl_capped() {
        let mut t = LeaseTable::default();
        let l = t
            .grant("rbd0".into(), None, 86_400, "x".into(), Some(0))
            .unwrap();
        assert_eq!(l.expires_unix, MAX_TTL_SECS);
    }
}
