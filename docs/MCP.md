<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# MCP and agent integration

Atlas exposes its read-only state to MCP hosts (Hermes Agent, Claude Code, …) at two endpoints.
Neither can fail over, delete, restore or wipe anything.

| Endpoint | Served by | Auth | Built with |
|---|---|---|---|
| `POST /api/atlas/v1/mcp` | `atlas-gateway` | the gateway's bearer JWT (operator or higher for most tools); optionally also an identity provider's JWT access token ([below](#idp-access-tokens-resource-server)) | `--features mcp` (included in `Dockerfile` / `Dockerfile.ceph`) |
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
| AI data (atlas_data) | Only when `ATLAS_DATA_URL` is set: `data_artifact_list`, `data_artifact_get`, `data_artifact_revisions`, `data_job_ready`, `data_workspace_review`, `data_cost_report`, `data_cost_estimate`; guarded writes (with `ATLAS_MCP_WRITE_ENABLE=1`): `data_workspace_change`, `data_workspace_publish`, `data_workspace_discard`, `data_job_release`. See [AI data tools](#ai-data-tools-atlas_data) |
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

### OAuth Protected Resource Metadata (RFC 9728)

The [MCP authorization spec](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)
says an MCP server MUST implement RFC 9728 and answer an unauthenticated request with a
`WWW-Authenticate` header carrying `resource_metadata`. Atlas does the following:

| Piece | Behaviour |
|---|---|
| `GET /.well-known/oauth-protected-resource/api/atlas/v1/mcp` | Public JSON for the MCP endpoint: `resource` (the MCP endpoint URL), `bearer_methods_supported: ["header"]`, `resource_name`, `resource_documentation`, and, **only when `ATLAS_MCP_IDP_ISSUER` is set**, `authorization_servers: ["<issuer>"]`. |
| `GET /.well-known/oauth-protected-resource` | The root form; `resource` is the origin (RFC 9728 section 3.3 requires it to match the resource the URL was built from). It never lists `authorization_servers`: Atlas enforces the MCP endpoint URL as the token audience, so a client that took the origin as its RFC 8707 `resource` would be issued a token Atlas refuses. |
| 401 on `/api/atlas/v1/mcp` | Adds `WWW-Authenticate: Bearer resource_metadata="<url of the first document>"`; when a token was sent and refused (bad signature, expired, revoked, wrong audience) it is `Bearer error="invalid_token", resource_metadata="..."`. The JSON body is unchanged. REST, gRPC and `/metrics` 401s get no header. |
| `/.well-known/mcp` | The card has `authentication.resource_metadata` (a path, relative like `endpoint`) and, with an IdP configured, `authentication.authorization_servers`. |

Only compiled with `--features mcp`, like the endpoint itself.

**Where the URL comes from.** Set `ATLAS_PUBLIC_URL` to the origin clients use (for example
`https://atlas.example.com`; no path, query or fragment; checked at startup, a bad value stops the gateway).
When set it is the only source, and request headers are ignored, so a forged `Host` cannot change what a
client is told. When unset the gateway falls back to the request's `Host` (accepted only if it is a plain
`host[:port]`) and `X-Forwarded-Proto` (only `https` is honoured, anything else is `http`). Both are chosen
by the caller unless a proxy overwrites them, so treat the fallback as a convenience for labs and NodePorts and set
`ATLAS_PUBLIC_URL` in production. Host-derived documents are sent `Cache-Control: no-store`. With no usable
`Host` and no `ATLAS_PUBLIC_URL`, the metadata routes return `400` and the 401 carries a bare `Bearer`.
The IdP access-token feature requires `ATLAS_PUBLIC_URL` (see below).

**What this still does not do.** Do not read it as full OAuth login support.

- There is **no OAuth authorization server** in Atlas: no Dynamic Client Registration, no Client ID Metadata
  Documents, no PKCE or token endpoint. A client authorizes against *your* identity provider (which must
  support what the spec asks of an authorization server, including audience binding), then calls Atlas with the
  access token it got. Atlas's own tokens are still HS256 JWTs minted by Atlas (`POST /auth/login`,
  `POST /auth/tokens`, the SSO console login); a client that cannot be given one out of band and has no IdP
  token cannot complete an OAuth flow against Atlas.
- No `scopes_supported` and no `scope` in the 401: Atlas has roles and tool/resource scopes, not OAuth scope
  strings, and an IdP token's `scope` claim is ignored. There is no `403 insufficient_scope` challenge; Atlas's
  403s (`FORBIDDEN`, tool denied) are unchanged and carry no `WWW-Authenticate`.
- **Opaque access tokens are not supported.** Only JWT access tokens (RS256 or ES256) are validated; Atlas does
  no token introspection (RFC 7662).
- The configured console OIDC issuer (`ATLAS_OIDC_ISSUER_URL`) is not advertised unless you also set
  `ATLAS_MCP_IDP_ISSUER` to it. The two settings are independent: console login mints an Atlas token and still
  works the same way.
- **Audience is enforced for IdP tokens only.** Atlas's own tokens carry no `aud` and keep working on every Atlas
  route unchanged (unless they are tool-scoped agent tokens, which are MCP only), so the spec's "validate that a
  token was issued for this server" is met for the tokens that come from an external authorization server, not for
  Atlas-minted ones.
- Not run against a real MCP client or a real identity provider: the tests exercise the HTTP behaviour and an
  in-process fake JWKS server.

Tests: `crates/atlas-gateway/tests/mcp.rs` (`oauth_*`, `mcp_401_*`, `rest_401_has_no_challenge`,
`authenticated_mcp_initialize_has_no_challenge`), `crates/atlas-gateway/tests/mcp_idp.rs` and the unit tests in
`src/oauth_resource.rs` and `src/idp_token.rs`.

### IdP access tokens (resource server)

With `ATLAS_MCP_IDP_ISSUER` set, `/api/atlas/v1/mcp` also accepts a **JWT access token issued by that identity
provider**, so an MCP host can authenticate a person through your IdP instead of being handed an Atlas token.
It is off by default. It applies to the MCP endpoint only: REST, gRPC and `/metrics` never accept an IdP token
(they validate Atlas HS256 tokens only), which the tests assert.

| Variable | Meaning |
|---|---|
| `ATLAS_MCP_IDP_ISSUER` | Issuer URL. `iss` must equal it exactly (a string, no trailing-slash tolerance). Unset: feature off. |
| `ATLAS_PUBLIC_URL` | **Required** with the above; the gateway refuses to start without it. The token's `aud` must contain `<ATLAS_PUBLIC_URL>/api/atlas/v1/mcp` (the `resource` of the metadata document). It is never taken from request headers. |
| `ATLAS_MCP_IDP_JWKS_URL` | Where to fetch signing keys. Unset: OIDC discovery (`<issuer>/.well-known/openid-configuration`; its `issuer` must equal the configured one) supplies `jwks_uri`. |
| `ATLAS_MCP_IDP_MAX_LIFETIME_SECS` | Longest validity accepted, default `86400`. `exp` may be at most this far ahead, and `exp - iat` (when `iat` is present) may not exceed it. |
| `ATLAS_MCP_IDP_ALLOW_HTTP` | `1` allows plain `http://` for the issuer, discovery and JWKS URLs to a non-loopback host (the Dex lab, [`deploy/dex-lab`](../deploy/dex-lab/README.md)). Default: `https` required, except loopback hosts. Never use it across an untrusted network: anyone who can alter the JWKS response can mint accepted tokens. |
| `ATLAS_OIDC_ADMIN_GROUP`, `ATLAS_OIDC_OPERATOR_GROUP`, `ATLAS_OIDC_TENANT_CLAIM` | The same group-to-role and tenant mapping the console SSO login uses (one shared function). `groups` containing an admin group is `admin`, else an operator group is `operator`, else `viewer`; a `role` claim is never read. The tenant is the named string claim, else `global` (admins are not tenant-limited). They need not be set together with `ATLAS_OIDC_ISSUER_URL`. |

What is checked: header `alg` must be `RS256` or `ES256` and a `kid` must be present; the key comes from the
issuer's JWKS by `kid` and the algorithm is pinned from the key (RSA verifies only RS256, P-256 only ES256), so
`alg: none`, every `HS*` (including the public key used as an HMAC secret), `PS*`, `ES384` and `EdDSA` are refused
and symmetric JWKS entries are never imported; signature; `iss`; `aud` (a string, or an array that contains the MCP
URL); `exp` (required); `nbf` (when present); `sub` (required, at most 256 characters); the lifetime cap above;
30 seconds of clock leeway. A refusal is a `401` with `WWW-Authenticate: Bearer error="invalid_token", ...` and a
short reason; the token itself is never logged or echoed.

Keys: fetched on first use, so an IdP that is down at startup does not stop the gateway (that request gets a 401 and
a log line, the next one retries). Cached for one hour. An unknown `kid` (key rotation) triggers a refetch, but at
most one fetch attempt per 30 seconds in total, failed ones included, so a flood of made-up `kid`s cannot make the
gateway hammer the IdP (a freshly rotated key can therefore be refused for up to 30 s). A successful fetch replaces the
whole key set, so a retired key stops being trusted at the next refresh. If a refresh fails, already cached keys are
used for up to one more hour, then requests get a 401. Responses over 256 KiB are refused, redirects are not
followed, RSA keys under 2048 bits and keys with a mismatching `alg`/`use` are skipped.

The resulting actor is **unscoped** (no tool allowlist, no resource scope, like an Atlas operator token) and limited
only by its role. Its id is `idp-<8 hex of sha256(issuer)>:<sub>`, so an IdP subject cannot take over an Atlas
subject's audit trail, rate-limit bucket or workspaces. A `jti` is recorded as `<same prefix>:<jti>` and checked
against the `POST /auth/tokens/{jti}/revoke` deny-list (revoke the prefixed value, which appears in the audit
`jti` field). **A token without a `jti` cannot be revoked in Atlas**; revoke it at the IdP and rely on the lifetime cap.

Limits to know: the IdP must put the MCP URL in `aud` (RFC 8707 `resource`; many IdPs need an API/resource
audience configured, and some, such as Dex, cannot set an arbitrary audience at all, see the Dex lab notes); group
membership is read from the token, so a role change at the IdP takes effect when the token is replaced; Atlas does
not check `azp`/`client_id`, any client the IdP issues a token to for this audience is accepted; there is no
per-client allow-list.

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
| `ATLAS_PUBLIC_URL` | gateway | Origin clients reach the gateway at (`https://atlas.example.com`), used for the RFC 9728 `resource` and the 401 `resource_metadata` URL ([above](#oauth-protected-resource-metadata-rfc-9728)). Unset: derived from `Host`/`X-Forwarded-Proto`. Invalid: the gateway refuses to start. Read once at startup. |
| `ATLAS_MCP_IDP_ISSUER` / `ATLAS_MCP_IDP_JWKS_URL` / `ATLAS_MCP_IDP_MAX_LIFETIME_SECS` / `ATLAS_MCP_IDP_ALLOW_HTTP` | gateway | Accept an identity provider's JWT access tokens on the MCP endpoint only; see [IdP access tokens](#idp-access-tokens-resource-server). Needs `ATLAS_PUBLIC_URL` (startup fails without it). Read once at startup. |
| `ATLAS_MCP_WRITE_ENABLE` | gateway | `1` lists the guarded `create_snapshot`, `ack_alert`, `silence_alert`, `create_workspace`, `reset_workspace` and `discard_workspace` tools. Off by default. |
| `ATLAS_MCP_AUDIT_CALLS` | gateway | Every tool call writes an `mcp.tool.call` audit row (caller, tool, `success` or `error`, token `jti`, role, whether the token is scoped), written after the response so it never delays a call. `0` turns it off, for a polling agent where one row per call is too many. Denied calls (`mcp.tool.denied`) and guarded writes are always audited. |
| `ATLAS_MCP_CALLS_PER_MIN` | gateway | Tool calls per minute each token may make (a token's own `jti`; callers without one share their actor id's budget). Calls over it are refused and audited as `mcp.tool.rate_limited`. Unset or `0` is off. It adds to the per-actor request limit (`ATLAS_RATE_LIMIT_RPM`), which already covers `/mcp`. The count is kept in process and shared through the database (see [HA.md](HA.md#mcp-call-budgets)), so on Postgres it holds across gateway pods within one sync interval; on SQLite it is per pod. Read once at startup. |
| `ATLAS_MCP_TOOL_CALLS_PER_MIN` | gateway | Calls per minute each token may make to any one tool, so a loop on one expensive tool cannot use up the whole `ATLAS_MCP_CALLS_PER_MIN` allowance. Calls over it are refused and audited as `mcp.tool.rate_limited` (with `scope: tool`). Only real tool names are counted. Unset or `0` is off. Shared through the database like `ATLAS_MCP_CALLS_PER_MIN`; read once at startup. |
| `ATLAS_AGENT_WORKSPACE_BYTES` | gateway | Default budget on the bytes of workspaces one caller may hold at once: the sum of the `max_bytes` of every workspace they own in the tenant, checked at create. A token's own `workspace_bytes` overrides it. Unset or `0` is off. While a budget applies `max_bytes` (or `ATLAS_WORKSPACE_DEFAULT_MAX_BYTES`) must be set, since an uncapped workspace cannot be counted. |
| `ATLAS_DATA_URL` / `ATLAS_DATA_TOKENS` / `ATLAS_DATA_TOKENS_FILE` / `ATLAS_DATA_TIMEOUT_SECS` | gateway | Enable and configure the `data_*` tools; see [AI data tools](#ai-data-tools-atlas_data). Unset URL: the tools do not exist. |
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
audit events ([AI_DATA_SERVICES.md](AI_DATA_SERVICES.md)), and the `data_*` tools forward the call's id to it ([below](#ai-data-tools-atlas_data)), so one run can be joined across both trails.

**Dry run.** The plan phase of every two-phase write already changes nothing and is the dry run, so no `dry_run` flag was added. One gap
was real: the `create_workspace` plan did not say whether the execute would be refused (tenant limit, size cap, byte budget), so the
token was spent to find out. The plan now carries `would_be_refused` with the reason, from the same checks the execute runs. The other
write tools (`create_snapshot`, `ack_alert`, `silence_alert`, `reset_workspace`, `discard_workspace`) have no such refusal to preview
beyond the validation their plan already does.

## AI data tools (atlas_data)

The tools call the separate `tools/atlas_data` service ([AI_DATA_SERVICES.md](AI_DATA_SERVICES.md)) over its `POST /v1/rpc/<action>` API.
They are **not listed at all** unless `ATLAS_DATA_URL` is set (an `http(s)` URL; the gateway does not follow redirects or use an
environment proxy). The four writes are listed only with `ATLAS_MCP_WRITE_ENABLE=1` and use the same two-phase, single-use token as the
other writes: the first call changes nothing and returns a plan that includes the workspace or job as atlas_data reports it right now (so an
unknown or not-visible target is rejected before any token is issued), and the token is bound to the caller, the tool and the exact
arguments. `data_workspace_change` sets (an `entry` whose chunks already exist in atlas_data; MCP cannot upload bytes) or deletes
(`delete: true`) one overlay path; `data_workspace_publish` creates a **new** immutable dataset from selected changed paths, owned by the
mapped atlas_data identity; `data_workspace_discard` closes a workspace; `data_job_release` frees a job's reservation. The base revision is
never modified. These are atlas_data workspaces (a manifest overlay), unrelated to the native-clone `list_workspaces` family above.

**Identity mapping.** atlas_data authenticates by bearer credential alone: the credential fixes its tenant, subject and role, and the action
envelope has no field for a caller-supplied subject (`Service.call` rejects extra arguments; pinned by
`tools/atlas_data/tests/test_mcp_contract.py`). The gateway therefore cannot pass the MCP caller's subject through; it can only choose which
credential to present. It holds a map, `ATLAS_DATA_TOKENS` (JSON object) or `ATLAS_DATA_TOKENS_FILE` (path to the same JSON, for a mounted
Secret; re-read on every call, so rotation needs no restart), keyed `actor:<gateway token subject>` then `tenant:<gateway tenant id>`:

```json
{"tenant:acme": "<credential for an atlas_data credential with tenant acme>", "actor:agent-7": "<narrower credential>"}
```

A caller with no entry is refused, with no fallback or default credential. atlas_data's own tenant and ACL checks then apply to whatever identity
that credential is, so a tenant-A caller cannot read tenant B's artifacts, jobs or workspaces through these tools (other tenants' resources
report "not found"). Limits, stated plainly: (1) all callers of one gateway tenant who share a `tenant:` entry act as one atlas_data subject
(use `actor:` entries for finer separation); (2) the gateway cannot verify that a credential's atlas_data tenant matches the key it is
filed under, or what its role is (there is no whoami action), so the operator must map tenants to same-tenant credentials and should use
`operator` (writes) or `viewer`-level credentials, never an `admin` one; (3) the gateway's own operator-role check gates the write tools
on top, but atlas_data's ACLs are what protect other subjects' data inside a tenant. Credentials are read only from the environment or the
file (the usual Secret mount; the Vault backend in [SECRETS.md](SECRETS.md) does not cover them), are sent only as the upstream bearer, and are
never logged, audited or returned; error text from atlas_data is scrubbed of the credential.

**Untrusted data.** Replies are wrapped as `{"source":"atlas_data","untrusted":true,"result":...}`, never interpreted, and refused (not
truncated) above 256 KiB. Upstream failures become short fixed messages: 401/403 "refused the credential", 404 "not found or not visible",
400/409/413 the (scrubbed, 200-character) atlas_data message, anything else "atlas_data failed"; timeouts (`ATLAS_DATA_TIMEOUT_SECS`, default 10,
1 to 60) and connection failures say so without naming the URL. TLS trust is the system store only; there is no CA-file option yet.

**Not exposed, because atlas_data cannot express it:** a workspace listing (only create/change/review/publish/discard exist, so the id must be
known), per-node readiness (only per-job `jobs.status`, which reports `data_ready` and `missing`), and KV-cache status or statistics (atlas_data
has only `kv.put`/`kv.get`, which move cache bytes, not stats). Dataset staging, node heartbeats, billing meters, rates, audit and
admin calls are also not tools. `data_cost_estimate` needs an operator-role credential upstream because atlas_data checks `write` for it.

**Run id.** The connection's `X-Atlas-Run-Id` (see the run id paragraph above) is forwarded to atlas_data on every request a data tool makes, as the
same `X-Atlas-Run-Id` header, including the read that builds a write plan and the execute call. atlas_data stores it on the audit events the call
writes (`audit.list` takes `run_id`), so one run reads as one trail across both services. A call without a run id sends no header. An invalid id never
gets this far: the gateway refuses the whole call first (above), and atlas_data would answer 400 to one regardless. No tool takes a `run_id` argument;
unknown argument fields are ignored and the id is never taken from one.

Tested over a fake atlas_data HTTP server (`crates/atlas-gateway/tests/mcp_data.rs`, `mcp_data_modes.rs`) plus a Python contract test of the
action names, argument names and reply fields. **Nothing ran against a deployed atlas_data and no agent was tried.**

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
