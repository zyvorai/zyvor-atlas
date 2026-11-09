// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! atlas_data MCP tool availability by configuration: unlisted without `ATLAS_DATA_URL`, read tools
//! only without `ATLAS_MCP_WRITE_ENABLE`, clean error for an unreachable service. Own test binary
//! (one test): the tool list is decided from process-wide environment when a session starts.
#![cfg(feature = "mcp")]

use serde_json::json;

mod common;
mod mcp_data_support;
use mcp_data_support::*;

fn data_tools(names: &[String]) -> Vec<&String> {
    names.iter().filter(|n| n.starts_with("data_")).collect()
}

#[tokio::test]
async fn tools_follow_configuration_and_fail_cleanly() {
    std::env::remove_var("ATLAS_DATA_URL");
    std::env::remove_var("ATLAS_MCP_WRITE_ENABLE");
    std::env::set_var("ATLAS_DATA_TOKENS", tokens_env());
    std::env::set_var("ATLAS_DATA_TIMEOUT_SECS", "1");
    let (addr, _state) = spawn().await;
    let token = mint("agent-a", "operator", "tenant-a", &[]);

    // Unconfigured: not listed, not callable, even with write enabled.
    let c = connect(addr, &token).await;
    assert!(data_tools(&tool_names(&c).await).is_empty());
    c.cancel().await.unwrap();
    std::env::set_var("ATLAS_MCP_WRITE_ENABLE", "1");
    let c = connect(addr, &token).await;
    assert!(data_tools(&tool_names(&c).await).is_empty());
    assert!(call(&c, "data_artifact_list", json!({})).await.is_err());
    c.cancel().await.unwrap();

    // Configured, writes off: read tools only; write tools are unlisted and refused.
    std::env::remove_var("ATLAS_MCP_WRITE_ENABLE");
    let (url, fake) = fake_atlas_data().await;
    std::env::set_var("ATLAS_DATA_URL", &url);
    let c = connect(addr, &token).await;
    let names = tool_names(&c).await;
    assert_eq!(data_tools(&names).len(), 7, "{names:?}");
    assert!(!names
        .iter()
        .any(|n| n == "data_workspace_discard" || n == "data_job_release"));
    assert!(
        call(&c, "data_workspace_discard", json!({"workspace_id": "ws1"}))
            .await
            .is_err()
    );
    assert_eq!(fake.count("workspaces.discard"), 0);
    assert!(call(&c, "data_artifact_list", json!({})).await.is_ok());
    c.cancel().await.unwrap();

    // Unreachable service: a short fixed error naming neither the URL nor the credential.
    std::env::set_var("ATLAS_DATA_URL", "http://127.0.0.1:1");
    let c = connect(addr, &token).await;
    let e = call(&c, "data_artifact_list", json!({})).await.unwrap_err();
    assert!(
        e.contains("unreachable") && !e.contains("127.0.0.1") && !e.contains(TOK_A),
        "{e}"
    );
    c.cancel().await.unwrap();
}
