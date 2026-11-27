// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureDomain {
    pub zone: String,
    pub rack: String,
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub failure_domain: FailureDomain,
    pub free_bytes: u64,
    pub healthy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementPolicy {
    pub replicas: usize,
    pub require_distinct_hosts: bool,
    pub prefer_distinct_racks: bool,
}

impl Default for PlacementPolicy {
    fn default() -> Self {
        Self {
            replicas: 3,
            require_distinct_hosts: true,
            prefer_distinct_racks: true,
        }
    }
}

pub fn select_replicas(nodes: &[Node], bytes: u64, policy: PlacementPolicy) -> Vec<String> {
    let mut candidates: Vec<&Node> = nodes
        .iter()
        .filter(|n| n.healthy && n.free_bytes >= bytes)
        .collect();
    candidates.sort_by(|a, b| {
        b.free_bytes
            .cmp(&a.free_bytes)
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut out = Vec::new();
    let mut hosts = std::collections::BTreeSet::new();
    let mut racks = std::collections::BTreeSet::new();

    // First pass maximizes rack diversity.
    if policy.prefer_distinct_racks {
        for n in &candidates {
            if out.len() == policy.replicas {
                break;
            }
            if policy.require_distinct_hosts && hosts.contains(&n.failure_domain.host) {
                continue;
            }
            if racks.contains(&n.failure_domain.rack) {
                continue;
            }
            out.push(n.id.clone());
            hosts.insert(n.failure_domain.host.clone());
            racks.insert(n.failure_domain.rack.clone());
        }
    }

    // Second pass fills remaining replicas while still respecting host diversity.
    for n in candidates {
        if out.len() == policy.replicas {
            break;
        }
        if out.iter().any(|id| id == &n.id) {
            continue;
        }
        if policy.require_distinct_hosts && hosts.contains(&n.failure_domain.host) {
            continue;
        }
        out.push(n.id.clone());
        hosts.insert(n.failure_domain.host.clone());
    }
    out
}
