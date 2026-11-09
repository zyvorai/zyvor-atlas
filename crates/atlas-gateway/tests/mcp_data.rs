// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! atlas_data MCP tools against a fake atlas_data HTTP server: identity mapping, tenant isolation,
//! size cap, upstream-error mapping without credential leaks, two-phase guarded writes and scoped
//! tokens. Own test binary (one test): ATLAS_DATA_* and the write gate are process-wide environment.
//! Nothing here ran against a real atlas_data.
#![cfg(feature = "mcp")]

use serde_json::json;

mod common;
mod mcp_data_support;
use mcp_data_support::*;

#[tokio::test]
async fn data_tools_map_identity_cap_responses_and_gate_writes() {
    let (url, fake) = fake_atlas_data().await;
    std::env::set_var("ATLAS_DATA_URL", &url);
    std::env::set_var("ATLAS_DATA_TOKENS", tokens_env());
    std::env::set_var("ATLAS_DATA_TIMEOUT_SECS", "1");
    std::env::set_var("ATLAS_MCP_WRITE_ENABLE", "1");
    let (addr, _state) = spawn().await;

    let a = connect(addr, &mint("agent-a", "operator", "tenant-a", &[])).await;
    let names = tool_names(&a).await;
    for want in [
        "data_artifact_list",
        "data_artifact_get",
        "data_artifact_revisions",
        "data_job_ready",
        "data_workspace_review",
        "data_cost_report",
        "data_cost_estimate",
        "data_workspace_change",
        "data_workspace_publish",
        "data_workspace_discard",
        "data_job_release",
    ] {
        assert!(
            names.contains(&want.to_string()),
            "missing {want}: {names:?}"
        );
    }

    // Reads return wrapped, untrusted-marked data, fetched with tenant A's credential only.
    let got = call(&a, "data_artifact_get", json!({"artifact_id": ART_A}))
        .await
        .unwrap();
    assert_eq!(got["source"], "atlas_data");
    assert_eq!(got["untrusted"], true);
    assert_eq!(got["result"]["id"], ART_A);
    assert_eq!(
        fake.seen.lock().unwrap().last().unwrap().bearer,
        format!("Bearer {TOK_A}")
    );
    let ready = call(&a, "data_job_ready", json!({"job": "job-a"}))
        .await
        .unwrap();
    assert_eq!(ready["result"]["data_ready"], false);
    let cost = call(&a, "data_cost_report", json!({"job": "job-a"}))
        .await
        .unwrap();
    assert_eq!(cost["result"]["micro_usd"], 7);
    let ws = call(&a, "data_workspace_review", json!({"workspace_id": "ws1"}))
        .await
        .unwrap();
    assert_eq!(ws["result"]["generation"], 3);
    assert!(
        call(&a, "data_cost_estimate", json!({"artifacts": [ART_A]}))
            .await
            .is_ok()
    );
    let revs = call(&a, "data_artifact_revisions", json!({"name": "ds"}))
        .await
        .unwrap();
    let ids: Vec<_> = revs["result"]["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["r1", "r2"],
        "exact-name match only, oldest first: {revs}"
    );
    // Bad arguments never reach the upstream.
    let before = fake.seen.lock().unwrap().len();
    assert!(
        call(&a, "data_artifact_get", json!({"artifact_id": "../x"}))
            .await
            .is_err()
    );
    assert!(
        call(&a, "data_workspace_review", json!({"workspace_id": "a b"}))
            .await
            .is_err()
    );
    assert_eq!(fake.seen.lock().unwrap().len(), before);

    // Tenant mapping: B's credential cannot see A's artifact or job, and vice versa; a tenant with no
    // credential is refused without any upstream call.
    let b = connect(addr, &mint("agent-b", "operator", "tenant-b", &[])).await;
    let r = call(&b, "data_artifact_get", json!({"artifact_id": ART_A})).await;
    assert!(
        r.unwrap_err().contains("not found"),
        "cross-tenant artifact"
    );
    assert!(call(&b, "data_job_ready", json!({"job": "job-a"}))
        .await
        .is_err());
    assert!(call(&b, "data_artifact_get", json!({"artifact_id": ART_B}))
        .await
        .is_ok());
    assert!(call(&a, "data_artifact_get", json!({"artifact_id": ART_B}))
        .await
        .is_err());
    let c = connect(addr, &mint("agent-c", "operator", "tenant-c", &[])).await;
    let before = fake.seen.lock().unwrap().len();
    let r = call(&c, "data_artifact_list", json!({})).await.unwrap_err();
    assert!(r.contains("no atlas_data credential"), "{r}");
    assert_eq!(
        fake.seen.lock().unwrap().len(),
        before,
        "no fallback credential may be tried"
    );
    b.cancel().await.unwrap();
    c.cancel().await.unwrap();

    // Oversized responses are refused, not truncated; upstream failures map to fixed messages that
    // carry neither the credential nor upstream internals; slow upstreams time out.
    let big = call(&a, "data_artifact_list", json!({"query": "huge"}))
        .await
        .unwrap_err();
    assert!(big.contains("size cap"), "{big}");
    let boom = call(&a, "data_artifact_list", json!({"query": "boom"}))
        .await
        .unwrap_err();
    assert!(!boom.contains("secret") && !boom.contains(TOK_A), "{boom}");
    let echo = call(&a, "data_artifact_list", json!({"query": "echo"}))
        .await
        .unwrap_err();
    assert!(
        echo.contains("[redacted]") && !echo.contains(TOK_A),
        "{echo}"
    );
    let slow = call(&a, "data_artifact_list", json!({"query": "slow"}))
        .await
        .unwrap_err();
    assert!(slow.contains("timed out"), "{slow}");

    // Guarded writes: the first call changes nothing, the token is bound to the arguments and single-use.
    let args = json!({"workspace_id": "ws1"});
    let plan = call(&a, "data_workspace_discard", args.clone())
        .await
        .unwrap();
    assert_eq!(plan["executed"], false);
    assert_eq!(plan["plan"]["current"]["generation"], 3);
    assert_eq!(
        fake.count("workspaces.discard"),
        0,
        "planning must not execute"
    );
    let token = plan["confirmation_token"].as_str().unwrap().to_string();
    let mut other = json!({"workspace_id": "ws2", "confirmation_token": token});
    assert!(
        call(&a, "data_workspace_discard", other.take())
            .await
            .is_err(),
        "token is bound to arguments"
    );
    assert_eq!(fake.count("workspaces.discard"), 0);
    let mut exec = args.clone();
    exec["confirmation_token"] = json!(token);
    let done = call(&a, "data_workspace_discard", exec.clone())
        .await
        .unwrap();
    assert_eq!(done["executed"], true);
    assert_eq!(done["result"]["result"]["state"], "discarded");
    assert_eq!(fake.count("workspaces.discard"), 1);
    assert!(
        call(&a, "data_workspace_discard", exec).await.is_err(),
        "replay refused"
    );
    assert_eq!(fake.count("workspaces.discard"), 1);

    // The other three writes plan first and execute with the issued token.
    let change = json!({"workspace_id": "ws1", "generation": 3, "path": "a.txt", "delete": true});
    let plan = call(&a, "data_workspace_change", change.clone())
        .await
        .unwrap();
    assert_eq!(plan["executed"], false);
    assert_eq!(fake.count("workspaces.change"), 0);
    let mut e = change.clone();
    e["confirmation_token"] = plan["confirmation_token"].clone();
    assert_eq!(
        call(&a, "data_workspace_change", e).await.unwrap()["executed"],
        true
    );
    assert_eq!(fake.count("workspaces.change"), 1);
    assert!(call(
        &a,
        "data_workspace_change",
        json!({"workspace_id": "ws1", "generation": 3, "path": "a.txt"})
    )
    .await
    .is_err());
    let publish = json!({"workspace_id": "ws1", "generation": 3, "name": "ds2", "revision": "v1", "paths": ["a.txt"]});
    let plan = call(&a, "data_workspace_publish", publish.clone())
        .await
        .unwrap();
    let mut e = publish.clone();
    e["confirmation_token"] = plan["confirmation_token"].clone();
    assert_eq!(
        call(&a, "data_workspace_publish", e).await.unwrap()["executed"],
        true
    );
    assert_eq!(fake.count("workspaces.publish"), 1);
    let release = json!({"job": "job-a"});
    let plan = call(&a, "data_job_release", release.clone()).await.unwrap();
    assert_eq!(plan["plan"]["current"]["node"], "n1");
    assert_eq!(fake.count("jobs.release"), 0);
    let mut e = release.clone();
    e["confirmation_token"] = plan["confirmation_token"].clone();
    assert_eq!(
        call(&a, "data_job_release", e).await.unwrap()["executed"],
        true
    );
    assert_eq!(fake.count("jobs.release"), 1);

    // A plan for another tenant's workspace is rejected before any token is issued.
    let r = call(&a, "data_workspace_discard", json!({"workspace_id": "ws2"}))
        .await
        .unwrap_err();
    assert!(r.contains("not found"), "{r}");
    a.cancel().await.unwrap();

    // Run id: nothing above sent one, so no request carried the header.
    assert!(
        fake.run_ids().iter().all(|r| r.is_empty()),
        "no run id, no header: {:?}",
        fake.run_ids()
    );
    // With one on the MCP connection, every atlas_data request of the call carries exactly that value:
    // a read, a body-carrying write plan (review) and the execute.
    let r = connect_with_run(
        addr,
        &mint("agent-a", "operator", "tenant-a", &[]),
        Some("run-7.a:b_c-9"),
    )
    .await;
    let before = fake.seen.lock().unwrap().len();
    call(&r, "data_artifact_get", json!({"artifact_id": ART_A}))
        .await
        .unwrap();
    let discard = json!({"workspace_id": "ws1"});
    let plan = call(&r, "data_workspace_discard", discard.clone())
        .await
        .unwrap();
    let mut exec = discard;
    exec["confirmation_token"] = plan["confirmation_token"].clone();
    assert_eq!(
        call(&r, "data_workspace_discard", exec).await.unwrap()["executed"],
        true
    );
    let seen = fake.seen.lock().unwrap().clone();
    assert!(seen.len() >= before + 3, "{seen:?}");
    for s in &seen[before..] {
        assert_eq!(
            s.run_ids,
            ["run-7.a:b_c-9"],
            "{} must carry the run id once",
            s.action
        );
        assert!(
            s.body.get("run_id").is_none(),
            "the id travels in the header, not the body: {s:?}"
        );
    }
    r.cancel().await.unwrap();
    // A longest-legal id is forwarded verbatim; an invalid one refuses the call before any request.
    let long = format!("L{}", "x".repeat(63));
    let r = connect_with_run(
        addr,
        &mint("agent-a", "operator", "tenant-a", &[]),
        Some(&long),
    )
    .await;
    let before = fake.seen.lock().unwrap().len();
    call(&r, "data_job_ready", json!({"job": "job-a"}))
        .await
        .unwrap();
    assert_eq!(fake.seen.lock().unwrap().last().unwrap().run_ids, [long]);
    r.cancel().await.unwrap();
    let r = connect_with_run(
        addr,
        &mint("agent-a", "operator", "tenant-a", &[]),
        Some("bad id"),
    )
    .await;
    let n = fake.seen.lock().unwrap().len();
    assert_eq!(n, before + 1);
    assert!(call(&r, "data_job_ready", json!({"job": "job-a"}))
        .await
        .unwrap_err()
        .contains("X-Atlas-Run-Id"));
    assert_eq!(
        fake.seen.lock().unwrap().len(),
        n,
        "an invalid run id never reaches atlas_data"
    );
    r.cancel().await.unwrap();

    // A viewer cannot plan a write, and still can read.
    let v = connect(addr, &mint("viewer", "viewer", "tenant-a", &[])).await;
    assert!(call(&v, "data_job_release", json!({"job": "job-a"}))
        .await
        .is_err());
    assert!(call(&v, "data_job_ready", json!({"job": "job-a"}))
        .await
        .is_ok());
    v.cancel().await.unwrap();

    // Scoped tokens: only the allowlisted tool is listed or callable.
    let s = connect(
        addr,
        &mint("scoped", "operator", "tenant-a", &["data_artifact_get"]),
    )
    .await;
    assert_eq!(tool_names(&s).await, ["data_artifact_get"]);
    assert!(call(&s, "data_artifact_get", json!({"artifact_id": ART_A}))
        .await
        .is_ok());
    let n = fake.seen.lock().unwrap().len();
    assert!(call(&s, "data_job_ready", json!({"job": "job-a"}))
        .await
        .is_err());
    assert!(call(&s, "data_job_release", json!({"job": "job-a"}))
        .await
        .is_err());
    assert_eq!(
        fake.seen.lock().unwrap().len(),
        n,
        "denied tools must not reach atlas_data"
    );
    s.cancel().await.unwrap();
}
