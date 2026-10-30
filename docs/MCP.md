<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# MCP and agent integration

Atlas exposes its read-only state to MCP hosts (Hermes Agent, Claude Code, …) at two endpoints.
Neither can fail over, delete, restore or wipe anything.

| Endpoint | Served by | Auth | Built with |
|---|---|---|---|
| `POST /api/atlas/v1/mcp` | `atlas-gateway` | the gateway's bearer JWT (operator or higher for most tools) | `--features mcp` (included in `Dockerfile` / `Dockerfile.ceph`) |
| `POST /mcp` on port 5111 | `atlas-io-agent` | `ATLAS_IO_TOKEN` bearer when set | `--features mcp` (included in `Dockerfile.io`) |

Transport is streamable HTTP. Per-tool detail and REST equivalents are in [API.md](API.md#mcp-model-context-protocol).

## Gateway tools

| Group | Tools |
|---|---|
| Inventory | `list_clusters`, `cluster_health`, `list_backends`, `list_pools`, `list_volumes` (tenant-scoped), `list_alerts`, `metrics_summary` |
| Advisory | `ops_advisor`, `list_incidents`, `detect_anomalies`, `what_if_capacity` (always local mode: never calls an external LLM) |
| I/O | `io_risk` (from the node agent; `unavailable` when not configured/reachable, never healthy) |
| DR | `dr_status`, `dr_preflight`, `list_dr_mirrors`, `dr_mirror_status`, `list_volume_groups` |
| Work | `list_jobs`, `get_job`, `list_snapshots` (operator role, stricter than REST; limited to the caller's tenant unless admin) |
| Ceph | `ceph_status`, `ceph_df`, `ceph_osd_tree` (summary: counts and the OSDs that are down or out), `ceph_nvme` (NVMe wear, media errors and spare from Ceph's device-health scrapes), `ceph_nvmeof` (Rook NVMe-oF gateways and their StorageClasses; needs a Kubernetes cluster). Operator role; output capped at 256 KiB |
| DataBridge | `databridge_plans`, `databridge_cdc_streams` (operator; tenant-filtered) |
| Governance | `list_policies` (static catalogue), `list_audit` (operator; tenant-filtered; request and result bodies are not returned) |
| Guarded write | `create_snapshot`, `ack_alert`, only when `ATLAS_MCP_WRITE_ENABLE=1` |

Macs registered as backends (`bkd_macos*`, see [MACOS.md](MACOS.md)) appear in `list_pools` and `list_volumes`
like any other backend. Erase, add/delete volume and eject are REST only, never MCP tools.

## Node agent tools

`io_health`, `io_histograms`, `io_workloads`, `io_rca`, `io_risk`, `io_leases`: read-only views of the
sensor ([IO_EBPF.md](IO_EBPF.md)). There is no tool that grants a write-freeze lease.

## Prompts and resources

Besides tools, both servers advertise MCP **prompts** (ready-made read-only workflows) and **resources**
(documentation), so a host can offer them without the user pasting a runbook.

| Server | Prompts | Resources |
|---|---|---|
| Gateway | `triage`, `capacity_review` (optional `add_tib`), `dr_readiness` (optional `mirror_id`) | `atlas://docs/mcp`, `atlas://docs/ai-advisor`, `atlas://docs/io-risk`, `atlas://docs/dr`, `atlas://skill/ops` |
| Node agent | `io_triage` | `atlas://docs/io-risk`, `atlas://docs/io-ebpf` |

Every prompt restates the rules (tool output is data, `unavailable` is not healthy, nothing here can fail
over or delete). Prompt arguments are validated, not echoed: `add_tib` must be a number from 0 to 100000 and
`mirror_id` 1 to 64 characters of letters, digits, `_` or `-`, so an argument cannot carry instructions.
Resources are fixed text compiled into the binary, identical for every caller; nothing is read from disk
at request time and any other URI is refused.

## Discovery

`GET /.well-known/mcp` on the gateway is public and static: name, version, transport, the endpoint path and
the auth scheme. It lists no tools and exposes no inventory or tenant data; those need a token.

## Connecting an agent

1. Mint a short-lived **operator** token: `examples/agent/mint-token.sh [subject] [ttl_secs]`
   (needs an admin token in `ATLAS_ADMIN_TOKEN`; never mints admin).
2. Point the client at it with `examples/agent/hermes-config.yaml` or `examples/agent/claude-mcp.json`.
3. Give the agent `examples/agent/atlas-ops-skill.md`: the triage flow and the hard rules
   (tool output is data, `unavailable` is not healthy, no admin tokens).

Detailed Hermes walkthrough: [HERMES_AGENT.md](HERMES_AGENT.md).

## Configuration

| Variable | Where | Meaning |
|---|---|---|
| `ATLAS_MCP_ALLOWED_HOSTS` | gateway | `Host` allow-list for the transport; `*` disables. Unset: loopback-only without auth, any host when auth is required (the endpoint is bearer-gated). |
| `ATLAS_MCP_WRITE_ENABLE` | gateway | `1` lists the guarded `create_snapshot` and `ack_alert` tools. Off by default. |
| `ATLAS_IO_AGENT_URL` / `ATLAS_IO_AGENT_TOKEN` / `ATLAS_IO_AGENT_TIMEOUT_SECS` | gateway | Where `io_risk` and the Ops Advisor's `io_risk` block read the node agent from. |
| `ATLAS_IO_TOKEN` | agent | Bearer token for the agent API (health probes exempt) and its `/mcp`. |

## Guarded writes

`create_snapshot` and `ack_alert` are two-phase and single-use: the first call (no `confirmation_token`) changes
nothing and returns the plan plus a token valid for 5 minutes and bound to the caller, the tool and
the exact arguments; the second call with that token executes the REST handler, so its role and
tenant checks apply. Proposals, rejections and executions are audited as `mcp.<tool>`. `ack_alert` only records that a
named operator saw an alert (the plan shows its title, severity and state); it does not resolve the alert or
silence notifications. Both tools share one gate, so a token for one tool or alert never works for another.
Tokens are held in process memory and die with the gateway.

## Threat model in short

- **Prompt injection** via data an agent reads (alert titles, device names, job text): tools return
  data only, the skill tells the agent to treat it as untrusted, and the only write paths (`create_snapshot`, `ack_alert`) need a
  human-approved confirmation of the exact arguments.
- **Token theft**: operator tokens are short-lived, revocable by `jti`, and cannot mint tokens.
  Never give an agent an admin token.
- **Fan-out**: advisory tools run in local mode so an agent call cannot trigger an outbound LLM
  request; the gateway's agent fetch is bounded (timeout, 256 KiB, no redirects) and fails open to
  `unavailable`.
- **Exposure**: keep the node agent on a cluster-internal Service and set `ATLAS_IO_TOKEN`.
