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
| I/O | `io_risk`, `io_ai` (from the node agent; `unavailable` when not configured/reachable, never healthy) |
| DR | `dr_status`, `dr_preflight`, `list_dr_mirrors`, `dr_mirror_status`, `list_volume_groups` |
| Work | `list_jobs`, `get_job`, `list_snapshots` (operator role, stricter than REST; limited to the caller's tenant unless admin) |
| Ceph | `ceph_status`, `ceph_df`, `ceph_osd_tree` (summary: counts and the OSDs that are down or out), `ceph_nvme` (NVMe wear, media errors and spare from Ceph's device-health scrapes), `ceph_nvmeof` (Rook NVMe-oF gateways and their StorageClasses; needs a Kubernetes cluster), `ceph_osd_objectstore` (each OSD's object store and Ceph version), `ceph_sizing` (NVMe hosts against a CPU/RAM rule of thumb). Operator role; output capped at 256 KiB |
| Operations | `osd_ok_to_stop` (would stopping these OSDs together keep every PG available; ask before maintenance), `ceph_health_rollup`, `metrics_forecast` (capacity runway), `upgrade_preflight`, `protection_status` (tenant-scoped), `policy_drift` and `orphan_backups` (both tenant-filtered), `list_events` (the jobs, audit and alerts feed; it has no tenant, so a tenant-scoped token is refused and told to use `list_audit`). Operator role; none of them changes anything |
| Native data | `native_reduction` (atlas-native compression and sharing savings; fails when the driver is off), `volume_ransomware` (activity-based verdict for one native filesystem volume, tenant-scoped; it never snapshots or locks, `POST …/ransomware/protect` stays admin REST only). Operator role |
| DataBridge | `databridge_plans`, `databridge_cdc_streams`, `databridge_validations` (optional `plan_id`), `databridge_cutovers` (operator; tenant-filtered) |
| Showback | `chargeback` — per-tenant usage and estimated cost; a tenant-bound token sees only its own row |
| Volumes | `get_volume`, `volume_protection` (by `id`) — operator, tenant-checked like the REST routes |
| Storage records | `list_buckets` (S3 secret is a reference only), `list_backups` and `list_schedules` (optional `volume_id`; no download URLs) — operator, tenant-filtered; `list_osds` (cluster-wide like the other Ceph tools; OSD rows carry no tenant) |
| Governance | `list_policies` (static catalogue), `list_audit` (operator; tenant-filtered; request and result bodies are not returned) |
| Agent workspaces | `list_workspaces`, `get_workspace`: disposable clones of native volumes and filesystems for agent runs ([NATIVE_FS.md](NATIVE_FS.md#agent-workspaces)); tenant-scoped like the REST routes |
| Guarded write | `create_snapshot`, `ack_alert`, `silence_alert`, and the workspace tools `create_workspace`, `reset_workspace`, `discard_workspace`, only when `ATLAS_MCP_WRITE_ENABLE=1` |

Macs registered as backends (`bkd_macos*`, see [MACOS.md](MACOS.md)) appear in `list_pools` and `list_volumes`
like any other backend. Erase, add/delete volume and eject are REST only, never MCP tools.

## Node agent tools

`io_health`, `io_histograms`, `io_workloads`, `io_rca`, `io_ai`, `io_risk`, `io_leases`, `io_heatmap` (columns for the last N minutes), `io_coverage` (BPF programs loaded and missing) and `io_native` (Atlas Native kernel maps; `unavailable` when they are not loaded, which is not healthy): read-only views of the
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

**Least privilege per agent.** An admin can mint a token limited to named tools with
`POST /auth/tokens {"subject": "...", "role": "operator", "tools": ["ceph_nvme", "ops_advisor"]}`
([API.md](API.md#post-apiatlasv1authtokens)). Such a token lists and calls only those tools, is
refused on every non-MCP route, and every refused call is audited as `mcp.tool.denied`. The
per-actor rate limit (`ATLAS_RATE_LIMIT_RPM`) already covers MCP calls, and a distinct subject gets
its own bucket. The scope is enforced in one place (`call_tool`/`list_tools` in
`crates/atlas-gateway/src/mcp.rs`), so new tools are covered automatically.

## Configuration

| Variable | Where | Meaning |
|---|---|---|
| `ATLAS_MCP_ALLOWED_HOSTS` | gateway | `Host` allow-list for the transport; `*` disables. Unset: loopback-only without auth, any host when auth is required (the endpoint is bearer-gated). |
| `ATLAS_MCP_WRITE_ENABLE` | gateway | `1` lists the guarded `create_snapshot`, `ack_alert`, `silence_alert`, `create_workspace`, `reset_workspace` and `discard_workspace` tools. Off by default. |
| `ATLAS_MCP_AUDIT_CALLS` | gateway | Every tool call writes an `mcp.tool.call` audit row (caller, tool, `success` or `error`, token `jti`, role, whether the token is scoped), written after the response so it never delays a call. `0` turns it off, for a polling agent where one row per call is too many. Denied calls (`mcp.tool.denied`) and guarded writes are always audited. |
| `ATLAS_MCP_CALLS_PER_MIN` | gateway | Tool calls per minute each token may make (a token's own `jti`; callers without one share their actor id's budget). Calls over it are refused and audited as `mcp.tool.rate_limited`. Unset or `0` is off. It adds to the per-actor request limit (`ATLAS_RATE_LIMIT_RPM`), which already covers `/mcp`. The count is kept in process and shared through the database (see [HA.md](HA.md#mcp-call-budgets)), so on Postgres it holds across gateway pods within one sync interval; on SQLite it is per pod. Read once at startup. |
| `ATLAS_MCP_TOOL_CALLS_PER_MIN` | gateway | Calls per minute each token may make to any one tool, so a loop on one expensive tool cannot use up the whole `ATLAS_MCP_CALLS_PER_MIN` allowance. Calls over it are refused and audited as `mcp.tool.rate_limited` (with `scope: tool`). Only real tool names are counted. Unset or `0` is off. Shared through the database like `ATLAS_MCP_CALLS_PER_MIN`; read once at startup. |
| `ATLAS_AGENT_WORKSPACE_BYTES` | gateway | Default budget on the bytes of workspaces one caller may hold at once: the sum of the `max_bytes` of every workspace they own in the tenant, checked at create. A token's own `workspace_bytes` overrides it. Unset or `0` is off. While a budget applies `max_bytes` (or `ATLAS_WORKSPACE_DEFAULT_MAX_BYTES`) must be set, since an uncapped workspace cannot be counted. |
| `ATLAS_IO_AGENT_URL` / `ATLAS_IO_AGENT_TOKEN` / `ATLAS_IO_AGENT_TIMEOUT_SECS` | gateway | Where `io_risk`, `io_ai` and the Ops Advisor's `io_risk` and `io_ai` blocks read the node agent from. |
| `ATLAS_IO_TOKEN` | agent | Bearer token for the agent API (health probes exempt) and its `/mcp`. |

## Guarded writes

`create_snapshot`, `ack_alert` and `silence_alert` are two-phase and single-use: the first call (no `confirmation_token`) changes
nothing and returns the plan plus a token valid for 5 minutes and bound to the caller, the tool and
the exact arguments; the second call with that token executes the REST handler, so its role and
tenant checks apply. Proposals, rejections and executions are audited as `mcp.<tool>`. `ack_alert` only records that a
named operator saw an alert (the plan shows its title, severity and state); it does not resolve the alert or
silence notifications. `silence_alert` silences webhook notifications for one alert for 60 s to 24 h (default 1 h; REST allows 30 days, so a longer window stays a human action); the alert is still tracked and listed, and the token is bound to the alert and the window. All write tools share one gate, so a token for one tool, alert or window never works for another.
Tokens are held in process memory and die with the gateway.

The workspace tools follow the same rules. `create_workspace` takes `source_volume_id` or `source_snapshot_id` (an atlas-native volume,
filesystem or snapshot in the caller's tenant), optional `name`, `ttl_secs` (60 s up to the server maximum, default 1 h) and `max_bytes`;
the plan shows the source and the limits, and the token is bound to all of them. The clone is a separate volume that expires on its own
(the gateway discards expired workspaces) and never writes to the source. `reset_workspace` rolls a workspace back to the state it was
created from, and `discard_workspace` deletes it with everything written to it. Both touch only the workspace's own clone and the snapshot
Atlas took for it, and a non-admin caller may only reset or discard a workspace they created themselves (REST lets any operator of the
tenant do it). The tools are not listed at all unless `ATLAS_MCP_WRITE_ENABLE=1`; `list_workspaces` and `get_workspace` are always listed.
Tenant quotas, per-tenant workspace limits and the TTL bounds are the REST handler's (`ATLAS_WORKSPACE_*`, see
[NATIVE_FS.md](NATIVE_FS.md#agent-workspaces)). A token scoped with `--tool` can call only the tools it lists. Tested over the fixture
driver (`crates/atlas-gateway/tests/mcp_workspaces.rs`); not run with a real agent or on the lab.

## Agent identity and guardrails

All of these are enforced at the one `call_tool` chokepoint, so they cover every tool, present and future. An unscoped token (no
`--tool`, no resource scope) behaves exactly as before.

**Resource scope.** `atlasctl issue-token agent-1 --role operator --volume vol_data1 --workspace 'ws_*' --snapshot 'snap_*' --path '/runs/1/*' --workspace-bytes 10737418240`
mints a token that may name only those volumes, snapshots, workspaces and paths (exact ids, or globs where `*` matches any run of
characters). Deny by default: once a scope is present, a tool argument naming a resource of a kind with no matching entry is refused
with `mcp.tool.denied` (reason, kind and the name, never the arguments). The arguments checked are `volume_id`,
`source_volume_id`, `snapshot_id`, `source_snapshot_id`, `workspace_id`, `path` and `paths`, plus `id` on `get_volume`,
`volume_protection` and `volume_ransomware`; a value of the wrong type is refused. Paths containing a `..` segment never match, so
`/runs/1/*` cannot be escaped. Tools that list the whole inventory (`list_volumes`, `list_snapshots`, `list_workspaces`,
`list_volume_groups`, `list_backups`, `list_schedules`, `protection_status`, `orphan_backups`, `list_events`, `list_audit`) cannot be
narrowed to a scope, so a scoped token is refused them unless the call names an in-scope resource (`list_backups` with an allowed
`volume_id`). A scope does not restrict tools that name no resource (`ceph_status`, `list_clusters`), so combine it with `--tool` to
limit those, and a tool whose resource argument has another name is not covered: when adding a tool, give resource arguments the names
above. A workspace's id is only known once it is created, so scope workspaces with a pattern such as `ws_*`. A token with a scope is
MCP-only, like a `--tool` token: it is refused on REST, gRPC and `/metrics`. Tested in
`crates/atlas-gateway/tests/mcp_guardrails.rs` (allowed and denied per resource and tool, unscoped unchanged, the audit rows, the issue-token endpoint).

**Budgets.** The per-token and per-tool calls-per-minute budgets share their counts through the database (see
[HA.md](HA.md#mcp-call-budgets)). `workspace_bytes` (the token claim, `--workspace-bytes`, or `ATLAS_AGENT_WORKSPACE_BYTES`) caps the sum of
`max_bytes` over the workspaces the token's subject holds; `create_workspace` over it is refused before the snapshot or clone is made, so
nothing is left behind. It counts the admitted caps, like `max_bytes` itself, not bytes actually written. The check is not atomic with the insert, so two creates racing can both pass, as with the per-tenant count.

**Run id.** Send `X-Atlas-Run-Id: <id>` on the MCP connection (1 to 64 characters of `[A-Za-z0-9_.:-]`, starting with a letter or digit; a
header, so a client sets it once per connection and no tool needs an argument). Every audit row the call causes carries it in the
`run_id` column (migration 0038): the `mcp.tool.call` and `mcp.<tool>` rows, refusals, and the rows the REST handlers a tool calls write
(`workspace.create`, `snapshot.create`). Filter with `list_audit` (`run_id`), `GET /audit?run_id=` and `atlasctl audit --run-id`. An invalid
id refuses the call and is audited without it. Rows written by work that runs on another task (background jobs a call enqueues) are not
tagged. The atlas_data service accepts the same id, as the `X-Atlas-Run-Id` header or a `run_id` field in an action body, on its own
audit events ([AI_DATA_SERVICES.md](AI_DATA_SERVICES.md)), so one run can be joined across both trails.

**Dry run.** The plan phase of every two-phase write already changes nothing and is the dry run, so no `dry_run` flag was added. One gap
was real: the `create_workspace` plan did not say whether the execute would be refused (tenant limit, size cap, byte budget), so the
token was spent to find out. The plan now carries `would_be_refused` with the reason, from the same checks the execute runs. The other
write tools (`create_snapshot`, `ack_alert`, `silence_alert`, `reset_workspace`, `discard_workspace`) have no such refusal to preview
beyond the validation their plan already does.

## Threat model in short

- **Prompt injection** via data an agent reads (alert titles, device names, job text): tools return
  data only, the skill tells the agent to treat it as untrusted, and the only write paths (`create_snapshot`, `ack_alert`, `silence_alert` and the workspace tools) need a
  human-approved confirmation of the exact arguments.
- **Token theft**: operator tokens are short-lived, revocable by `jti`, and cannot mint tokens. A tool-scoped token also cannot be used outside MCP. Mint one with `atlasctl issue-token agent-1 --role viewer --tool ceph_nvme --tool io_ai` (admin; repeat `--tool`).
  Never give an agent an admin token.
- **Fan-out**: advisory tools run in local mode so an agent call cannot trigger an outbound LLM
  request; the gateway's agent fetch is bounded (timeout, 256 KiB, no redirects) and fails open to
  `unavailable`.
- **Exposure**: keep the node agent on a cluster-internal Service and set `ATLAS_IO_TOKEN`.
