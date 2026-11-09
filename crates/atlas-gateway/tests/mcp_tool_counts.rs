// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! How many MCP tools a session lists for each configuration gate, so the number quoted in the docs
//! (`docs/STATUS.md`, `docs/COMPARISON.md`, `docs/ROADMAP.md`, the website) is the real one. Own test
//! binary (one test): the gates are process-wide environment variables read when a session starts.
//!
//! The counts, with their sources:
//!
//! | gate                                     | tools | made of                                   |
//! |------------------------------------------|-------|-------------------------------------------|
//! | default (read-only)                      | 53    | `mcp.rs` 21 + `mcp/ops.rs` 30 + 2 workspace reads |
//! | `ATLAS_MCP_WRITE_ENABLE`                 | 59    | + 3 guarded writes + 3 workspace writes   |
//! | `ATLAS_DATA_URL`                         | 60    | default + 7 `atlas_data` reads            |
//! | both                                     | 70    | write + 7 reads + 4 `atlas_data` writes   |
#![cfg(feature = "mcp")]

use std::collections::HashSet;

mod common;
mod mcp_data_support;
use mcp_data_support::*;

const COUNT_READ_ONLY: usize = 53;
const COUNT_WITH_WRITES: usize = 59;
const COUNT_WITH_DATA: usize = 60;
const COUNT_WITH_BOTH: usize = 70;

fn tool_attrs(src: &str) -> usize {
    src.matches("#[tool(").count()
}

#[test]
fn source_attribute_counts_match_the_documented_breakdown() {
    // The documented figures are sums of these files' `#[tool(` attributes.
    let core = tool_attrs(include_str!("../src/mcp.rs"));
    let ops = tool_attrs(include_str!("../src/mcp/ops.rs"));
    let ws = tool_attrs(include_str!("../src/mcp/workspaces.rs"));
    let write = tool_attrs(include_str!("../src/mcp/write.rs"));
    let data = tool_attrs(include_str!("../src/mcp/data.rs"));
    assert_eq!((core, ops, ws, write, data), (21, 30, 5, 3, 11));
    assert_eq!(
        core + ops + 2,
        COUNT_READ_ONLY,
        "2 of the 5 workspace tools are reads"
    );
    assert_eq!(COUNT_READ_ONLY + write + 3, COUNT_WITH_WRITES);
    assert_eq!(COUNT_READ_ONLY + 7, COUNT_WITH_DATA);
    assert_eq!(COUNT_WITH_WRITES + data, COUNT_WITH_BOTH);
}

#[tokio::test]
async fn listed_tools_follow_the_write_and_data_gates() {
    std::env::remove_var("ATLAS_DATA_URL");
    std::env::remove_var("ATLAS_MCP_WRITE_ENABLE");
    std::env::set_var("ATLAS_DATA_TOKENS", tokens_env());
    let (addr, _state) = spawn().await;
    // An operator token with no tool allow-list sees every tool the server lists.
    let token = mint("agent-counts", "operator", "tenant-a", &[]);

    async fn listed(addr: std::net::SocketAddr, token: &str) -> Vec<String> {
        let c = connect(addr, token).await;
        let names = tool_names(&c).await;
        c.cancel().await.unwrap();
        names
    }
    let unique = |names: &[String]| names.iter().collect::<HashSet<_>>().len();

    // Default: read-only, no atlas_data.
    let base = listed(addr, &token).await;
    assert_eq!(base.len(), COUNT_READ_ONLY, "{base:?}");
    assert_eq!(unique(&base), base.len(), "no tool is listed twice");
    for w in [
        "create_snapshot",
        "ack_alert",
        "create_workspace",
        "reset_workspace",
        "discard_workspace",
    ] {
        assert!(
            !base.iter().any(|n| n == w),
            "{w} is a write tool: {base:?}"
        );
    }
    assert!(!base.iter().any(|n| n.starts_with("data_")));

    // Writes on: three guarded writes and three workspace writes.
    std::env::set_var("ATLAS_MCP_WRITE_ENABLE", "1");
    let writes = listed(addr, &token).await;
    assert_eq!(writes.len(), COUNT_WITH_WRITES, "{writes:?}");
    assert_eq!(unique(&writes), writes.len());
    assert!(
        base.iter().all(|n| writes.contains(n)),
        "write mode only adds tools"
    );
    let added: Vec<_> = writes.iter().filter(|n| !base.contains(n)).collect();
    assert_eq!(added.len(), 6, "{added:?}");

    // atlas_data configured, writes off: seven read tools.
    std::env::remove_var("ATLAS_MCP_WRITE_ENABLE");
    let (url, _fake) = fake_atlas_data().await;
    std::env::set_var("ATLAS_DATA_URL", &url);
    let data_reads = listed(addr, &token).await;
    assert_eq!(data_reads.len(), COUNT_WITH_DATA, "{data_reads:?}");
    assert_eq!(
        data_reads.iter().filter(|n| n.starts_with("data_")).count(),
        7
    );

    // Both: the four atlas_data writes join.
    std::env::set_var("ATLAS_MCP_WRITE_ENABLE", "1");
    let both = listed(addr, &token).await;
    assert_eq!(both.len(), COUNT_WITH_BOTH, "{both:?}");
    assert_eq!(unique(&both), both.len());
    assert_eq!(both.iter().filter(|n| n.starts_with("data_")).count(), 11);
}
