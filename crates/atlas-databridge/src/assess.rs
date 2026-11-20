// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Migration readiness scoring. Turns a discovered schema into a 0–100 score plus blockers and
//! warnings, so an operator sees what stands between the source and a clean cutover before committing.

use serde::{Deserialize, Serialize};

use crate::connector::DiscoveredSchema;

/// Extensions that don't exist (or behave differently) on a vanilla edge Postgres/MySQL — each is a
/// blocker until the operator confirms an equivalent is installed on the target.
const RISKY_EXTENSIONS: &[&str] = &["postgis", "timescaledb", "citus", "pg_partman"];

/// 1 GiB.
const GIB: i64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assessment {
    pub score: i64,
    pub risk: String, // low | medium | high
    pub blockers: Vec<String>,
    pub warnings: Vec<String>,
    pub downtime_estimate: String,
    pub total_size_bytes: i64,
    pub table_count: usize,
}

/// Score a discovered schema. Higher is better; blockers subtract the most.
pub fn assess(schema: &DiscoveredSchema) -> Assessment {
    let mut score: i64 = 100;
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    // CDC needs logical decoding / binlog on the source.
    if !schema.cdc_capable {
        score -= 30;
        blockers.push(
            "source has no logical decoding / binlog enabled — required for continuous CDC".into(),
        );
    }

    // CDC needs a primary key (or replica identity) per table.
    let no_pk: Vec<&str> = schema
        .tables
        .iter()
        .filter(|t| !t.has_primary_key)
        .map(|t| t.name.as_str())
        .collect();
    if !no_pk.is_empty() {
        score -= 10 * no_pk.len().min(4) as i64;
        blockers.push(format!(
            "{} table(s) without a primary key (no CDC replica identity): {}",
            no_pk.len(),
            no_pk.join(", ")
        ));
    }

    // Extensions the edge target may not have.
    for ext in &schema.extensions {
        if RISKY_EXTENSIONS.iter().any(|r| ext.eq_ignore_ascii_case(r)) {
            score -= 15;
            blockers.push(format!(
                "extension '{ext}' must be installed on the edge target before migration"
            ));
        }
    }

    // Large tables slow the full-load; a warning, not a blocker.
    for t in &schema.tables {
        if t.size_bytes > 5 * GIB {
            warnings.push(format!(
                "table {} is large ({} GiB) — full-load will take a while",
                t.name,
                t.size_bytes / GIB
            ));
        }
    }
    if schema.total_size_bytes > 100 * GIB {
        warnings.push("total dataset > 100 GiB — plan the full-load window accordingly".into());
    }

    let score = score.clamp(0, 100);
    let risk = if score >= 85 {
        "low"
    } else if score >= 60 {
        "medium"
    } else {
        "high"
    };

    Assessment {
        score,
        risk: risk.into(),
        blockers,
        warnings,
        downtime_estimate: downtime_estimate(schema.total_size_bytes),
        total_size_bytes: schema.total_size_bytes,
        table_count: schema.tables.len(),
    }
}

/// Very rough cutover-downtime estimate: with CDC, downtime is just the final drain + switch, so it
/// scales with lag not full size. This is the operator-facing ballpark.
fn downtime_estimate(total_size_bytes: i64) -> String {
    let gib = total_size_bytes / GIB;
    if gib < 50 {
        "2–5 minutes".into()
    } else if gib < 500 {
        "5–15 minutes".into()
    } else {
        "15–30 minutes".into()
    }
}
