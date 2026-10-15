<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Application recovery plans and restore drills

`tools/atlas_recovery` is an operator-run, Python 3.10+ tool using Atlas's existing
REST API. It needs no Python packages, server migration, privileges on the gateway,
or additional network listener. It restores snapshots into **new** volumes, verifies
them, and cleans them up. It never rolls back or deletes a source volume.

## Quickstart

From the repository root:

```bash
export PYTHONPATH="$PWD/tools"
# Supply an operator token through ATLAS_TOKEN in your process environment.
python3 -m atlas_recovery validate --plan examples/recovery/native-block.json
python3 -m atlas_recovery preflight --plan /etc/atlas-recovery/plan.json \
  --base-url https://atlas.example.com/api/atlas/v1
python3 -m atlas_recovery drill --plan /etc/atlas-recovery/plan.json \
  --base-url https://atlas.example.com/api/atlas/v1 \
  --report-dir /var/lib/atlas-recovery/reports
```

Replace the example resource IDs and checksum with your actual resources. The
Native example checks the 20-byte `atlas-recovery-proof` marker at offset zero;
it is illustrative, not a digest for arbitrary user data. Write it only to a
throwaway test volume. Checks hash restored bytes against a **pre-established**
expected SHA-256 value, never against a changing production volume.

Remote gateways require HTTPS with normal CA and hostname verification. Use
`--ca-file /path/to/ca.pem` for a private CA. HTTP is accepted only on literal
loopback addresses or localhost. Redirects are refused. The token is neither
printed nor included in reports or hook subprocess environments.

Exit codes: `0` verified success, `1` failed drill/capture or missed objective,
`2` input, transport or preflight error. An input/preflight error may happen
before any report is created. Monitoring should check the process exit code.

## Plan contract

See `examples/recovery/application.json` for a database followed by an API.

- `version`: `1`.
- `name`: lowercase identifier, at most 30 characters.
- `tenant_id`: the tenant owning every source volume and snapshot.
- `target_namespace`: a DNS label beginning with `atlas-drill-`, distinct from
  every source namespace. Provision this namespace before a Ceph/PVC drill.
- `rpo_seconds`: maximum age of the oldest selected snapshot, default 3600.
- `rto_seconds`: maximum elapsed clone/start/verification time, default 600.
  It excludes cleanup and read-only preflight. This is a drill recovery duration,
  not a promise about recovery from a site-wide outage.
- `capture_timeout_seconds`: capture budget, default 120, maximum 600. The runner
  checks the remaining budget between hooks/requests and limits each snapshot
  request/job wait to the remaining budget. Resume hooks always run separately.
- `components`: 1–64 components, unique IDs, optional `depends_on`, 1–16 volumes
  per component. Unknown dependencies, cycles and reused source volumes fail
  validation. Restoration and start follow dependency order; stop is reversed.
- Each volume supplies `volume_id` and `snapshot_id`. Use `"latest"` to choose
  the newest ready snapshot for that exact volume and tenant at preflight.
  The selection is pinned for the run; resolved IDs are recorded in the report.
- Every volume must have one or more `checks`, or its component must have a
  `verify_hook`. An inventory-only check can never pass a drill.

Checksum ranges contain `offset`, `length` (1 byte–4 MiB), and lowercase `sha256`.
They currently work only for Native **block** volumes through Atlas's raw data
API. Reports identify the exact ranges tested; a sampled digest is not a full
volume scrub. Use application verification hooks for Ceph/PVCs and Native
filesystems. Backend capability preflight currently permits Ceph and Native;
read-only Longhorn/NFS/ZFS/WEKA integrations are refused.

Snapshots are validated against the tenant-filtered volume inventory and snapshot
catalog. Missing, unready, foreign, undated, or implausibly future-dated snapshots
fail before restoring. SQLite timestamps without offsets are interpreted as UTC.
An RPO violation creates a failed report without making mutations. RTO violations
also fail the drill even if restored data was otherwise valid.

## Application hooks

Hook commands are installed by the operator and referenced by name in plans.
`examples/recovery/hooks.json` shows the registry; these executable paths are
**placeholders to replace**, not built-in database implementations.

Each registry entry contains an `argv` array with an absolute executable path,
allowed `phases`, and `timeout_seconds` (default 30, maximum 600). There is no
shell expansion, interpolation, or command text from the plan. Keep the registry
and executables operator-owned and unwritable by untrusted plan authors.

The runner sends one JSON object on stdin: `phase`, `run_id`, `tenant_id`,
`namespace`, `component_id`, source IDs, and this component's restored resource
records (IDs, names, snapshot IDs, job IDs). Hooks return zero only on success.
Output is discarded to prevent credentials appearing in reports. The environment
contains only PATH/locale/platform basics and `ATLAS_RECOVERY_PHASE`. Hooks needing
Kubernetes or application credentials should load dedicated files explicitly.

Implementations must use the supplied **clone** IDs and target namespace for
start/verify/stop, and validate the actual application state (for example, database
recovery completed plus a known query). `start_hook` requires a paired `stop_hook`.
A partially failed start still invokes stop. If any stop fails, clones are retained
and the drill fails, rather than deleting volumes still used by a workload.

Namespace naming and separate clones do not establish network isolation. Before
using application hooks, provision deny-by-default ingress/egress policies and
restricted service accounts, and avoid production Service names, credentials,
external side effects and source-volume mounts. Native volumes do not live in a
Kubernetes namespace: the hook must attach their distinct clone IDs explicitly.
The runner cannot prove that a custom executable honors these contracts.

## Coordinated snapshot capture

```bash
python3 -m atlas_recovery capture --plan /etc/atlas-recovery/source-plan.json \
  --hooks /etc/atlas-recovery/hooks.json \
  --base-url https://atlas.example.com/api/atlas/v1 \
  --report /var/lib/atlas-recovery/capture-001.json \
  --captured-plan /var/lib/atlas-recovery/captured-001.json
```

Capture allows volume entries without snapshot IDs. It quiesces consumers before
providers, creates snapshots, then resumes providers before consumers, including
a component whose quiesce hook failed partway. Quiesce requires a paired resume.
Capture **can pause source applications**: schedule it as a maintenance operation
with reviewed hooks. For normal recurring drills, use already scheduled Atlas
snapshots and `snapshot_id: "latest"`.

A successful all-hook capture is reported as `hook_coordinated`; without a hook
for every component it is `crash`. Snapshots are taken sequentially and remain
marked crash-consistent by Atlas. Hook coordination is not a storage-atomic group
snapshot or proof of database consistency. Verification must establish that.
Created snapshots are retained, including partial captures, so an operator can
inspect or apply retention policies. Capture reports list every known resource.
A resume failure always fails the capture, and no captured-plan output is emitted.
SIGTERM attempts orderly cleanup. SIGKILL, power loss or host loss cannot execute
resume hooks; quiesce implementations should have their own bounded leases or
external watchdog. Capture hooks must fit the configured capture budget.

## Durable reports and failure handling

Reports use private files, atomic replacement, and fsync of file contents after
phase changes. Each run reserves a unique report path; an existing path is refused.
Use a durable local volume for journals. Reports include resources, hook/check
outcomes, RPO/RTO results, cleanup failures, unresolved mutations and timestamps.
The JSON schema version is `1`. Checks and hooks are recorded separately; byte
checks alone do not establish application startup.

The mutation intent is persisted **before** sending a create. Mutations are not
retried after ambiguous responses. `uncertain_operations` means a resource may
have been created even though its ID was not received. An async clone timeout
also leaves the resource/job in the report; a still-running clone job is never
followed by a premature volume deletion. Operators should reconcile the recorded
unique names and job IDs through Atlas, stop any drill workloads, and remove
only their drill resources after jobs reach terminal states. Source snapshots
are never deleted by drill cleanup. Cleanup uses recorded clone IDs, rechecks
names, excludes all plan source IDs, and never uses force deletion. A failed
cleanup makes the whole drill fail.

Reports are a local journal, not an HA workflow engine. A process crash does not
automatically resume a run. Keep sensitive hooks/configuration out of reports;
local administrators can inspect resource metadata in the journal. Apply your
own report retention policy.

## Scheduling and verification

The example systemd service/timer runs a daily drill at 02:00 UTC with a unique
report per invocation. Install Python, the repository, dedicated credentials,
operator-owned hooks/config, the service user and writable reports directory
first. Replace the gateway URL and provision hook-specific access. Use
`snapshot_id: "latest"` for recurring drills. Configure monitoring for nonzero
service exits and stale/missing reports; the timer does not send notifications.

```bash
PYTHONPATH=tools python3 -m unittest discover -s tools/atlas_recovery/tests -v
cargo test -p atlas-io --locked
cargo clippy -p atlas-io --all-targets --locked -- -D warnings
```

The Python suite uses an HTTP contract fixture and real subprocess hooks; it does
not claim Ceph, Kubernetes network isolation or database-engine certification.
`tests/live_native.py` additionally exercises the actual gateway and Native node
binaries on loopback using temporary storage; build the binaries first and run
it with `PYTHONPATH=tools python3 tools/atlas_recovery/tests/live_native.py`.

## I/O diagnosis

`GET /io/rca` keeps its existing response schema and p50/p99 service histogram.
Notes now distinguish observed scheduler queue pressure, device service latency,
and mixed pressure. `insert → issue` wait is **added** to `issue → complete`
service averages; it is never subtracted from the service histogram or presented
as an end-to-end percentile. Fewer than 20 samples do not infer a queue cause;
missing queue observations and inconsistent queue counters are explicit. The
hottest issuer is restricted to the attributed volume when known. These are
observational hints, not assertions that a particular disk or OSD failed.
