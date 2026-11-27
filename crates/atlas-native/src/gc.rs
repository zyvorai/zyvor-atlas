// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

use crate::metadata::{Catalog, ExtentId};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct GcStats {
    pub candidates: u64,
    pub reclaimed: u64,
    /// Device bytes (summed over replicas) returned to the free lists.
    pub freed_bytes: u64,
}

pub fn collect_candidates(catalog: &Catalog) -> Vec<ExtentId> {
    catalog
        .extents
        .iter()
        .filter(|(_, e)| e.refs == 0 && !e.tombstoned)
        .map(|(id, _)| id.clone())
        .collect()
}
