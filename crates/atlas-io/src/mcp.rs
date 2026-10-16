// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Read-only MCP view of the agent (`/mcp`, streamable HTTP). Same data and the same
//! never-fabricate rules as the REST routes; there are deliberately no tools that grant a
//! write-freeze lease or otherwise change anything.

use std::sync::Arc;

use rmcp::{
    handler::server::{
        router::tool::ToolRouter,
        wrapper::{Json as ToolJson, Parameters},
    },
    model::{ErrorData as McpError, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{score, Collector};

#[derive(Clone)]
pub struct IoMcp {
    collector: Arc<Collector>,
    #[allow(dead_code)]
    tool_router: ToolRouter<IoMcp>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct VolumeArgs {
    /// Optional volume id filter (as shown in `io_rca` / `io_histograms`).
    #[serde(default)]
    volume: Option<String>,
}

#[tool_router]
impl IoMcp {
    pub fn new(collector: Arc<Collector>) -> Self {
        Self {
            collector,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Sensor health: mode (fake/live), loaded and missing BPF programs, events \
                        seen/dropped. A live agent that could not attach reports its programs \
                        missing rather than inventing data."
    )]
    async fn io_health(&self) -> Result<ToolJson<Value>, McpError> {
        Ok(ToolJson(json!(self.collector.health())))
    }

    #[tool(description = "Per-(device, op) latency histograms with p50/p99 and queue time.")]
    async fn io_histograms(&self) -> Result<ToolJson<Value>, McpError> {
        self.collector.poll();
        Ok(ToolJson(json!(self.collector.histograms())))
    }

    #[tool(description = "Per-process/cgroup workload accounting by device.")]
    async fn io_workloads(&self) -> Result<ToolJson<Value>, McpError> {
        self.collector.poll();
        Ok(ToolJson(json!(self.collector.workloads())))
    }

    #[tool(
        description = "Root-cause hints per device from the histograms: verdict and cause= notes. \
                        Observational evidence, not proof of a failing component."
    )]
    async fn io_rca(
        &self,
        Parameters(args): Parameters<VolumeArgs>,
    ) -> Result<ToolJson<Value>, McpError> {
        self.collector.poll();
        Ok(ToolJson(json!(self.collector.rca(args.volume.as_deref()))))
    }

    #[tool(
        description = "Overall I/O risk level (healthy/elevated/critical) over the current RCA, \
                        with the evidence behind it. Read-only; can_execute is always false."
    )]
    async fn io_risk(
        &self,
        Parameters(args): Parameters<VolumeArgs>,
    ) -> Result<ToolJson<Value>, McpError> {
        self.collector.poll();
        let rca = self.collector.rca(args.volume.as_deref());
        let level = score(&rca, None);
        Ok(ToolJson(json!({
            "level": level.as_str(),
            "can_execute": false,
            "evidence": rca.iter().map(|r| json!({
                "device": r.device,
                "volume_id": r.volume_id,
                "verdict": r.verdict,
                "notes": r.notes,
            })).collect::<Vec<_>>(),
        })))
    }

    #[tool(description = "Active and recent write-freeze leases (read-only view).")]
    async fn io_leases(&self) -> Result<ToolJson<Value>, McpError> {
        Ok(ToolJson(json!(self.collector.leases())))
    }
}

#[tool_handler]
impl ServerHandler for IoMcp {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "Atlas storage I/O sensor — read-only latency, attribution, RCA and risk tools. \
             Nothing here changes the system."
                .into(),
        );
        info
    }
}

/// MCP service for the agent router. `authenticated` is true when the router enforces a bearer
/// token; only then is rmcp's loopback-only `Host` check lifted (without a token an agent that
/// is reachable on the network must not also accept arbitrary Host headers).
pub fn service(
    collector: Arc<Collector>,
    authenticated: bool,
) -> StreamableHttpService<IoMcp, LocalSessionManager> {
    let cfg = StreamableHttpServerConfig::default();
    let cfg = if authenticated {
        cfg.disable_allowed_hosts()
    } else {
        cfg
    };
    StreamableHttpService::new(
        move || Ok(IoMcp::new(collector.clone())),
        LocalSessionManager::default().into(),
        cfg,
    )
}
