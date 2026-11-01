<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Atlas AI data services

This change delivers all nine proposed AI data workflows together as the `atlas_data`
Python companion service, CLI and SDK. It uses real files, SHA-256 content chunks,
a durable SQLite catalog (or Postgres, below), application verification processes, and optional Apache
Iceberg/Parquet. It does not change Atlas Native's format or the Rust gateway.

**Status: experimental, local verification only; one service process by default, several replicas with a Postgres catalog.** This is a
working first implementation, not production parity with ONTAP or a performance claim.

## Capability and boundary

| Workflow | Implemented | Boundary |
|---|---|---|
| Shared model cache | Immutable model revisions, deduplicated 4 MiB chunks per tenant, resumable chunk uploads, verified exports, shared worker chunk cache, pinned placements | No Hugging Face proxy or peer-to-peer transport; re-running ingest resumes by deduplicating chunks already uploaded |
| Data-ready scheduling | Label/health/capacity filtering, transfer-time ranking, transactional reservations, readiness gate, stage worker, lease renew/release | External scheduler must use `DataReadyGate`; no automatic Gryvia controller or Kubernetes scheduler plugin |
| Agent workspaces | Copy-on-write manifests, path overlays/tombstones, optimistic generations, byte/time budgets, review, selective publication, discard | Branch is a virtual file manifest, not an OS sandbox or writable FUSE mount; export published artifacts into FluxVM separately |
| Permission-aware data access | Tenant and subject identity from credentials, artifact ownership, live ACL revocation across revisions, filename/name search, authorized chunk retrieval, audit trail | Source ACLs must be imported explicitly; no automatic AD/SMB ACL synchronization, semantic embeddings, OCR or vector database |
| Inference context cache | Exact compatibility keys, subject isolation, TTL, 64 MiB subject budget with LRU eviction, byte-block SDK | Blocks <=4 MiB, no tensor conversion, NIXL transport, GPU residency or drop-in vLLM/MLX connector; adapter required in serving runtime |
| Dataset releases | Immutable file manifests and checksums, provenance, metadata-only branches, revision diff, verified exports | A release captures uploaded files, not an automatically quiesced database; use existing `atlas_recovery` for coordinated live capture |
| Application-verified recovery | Isolated temporary restore, all-file checksum validation, allowlisted verification process, deadline, durable pass/fail evidence, latest verified release | Hook must actually start/check/stop its application; no isolation from a malicious trusted hook, no malware-clean certification; snapshot age is not measured RPO |
| Per-job storage costs | Trusted collectors, integer micro-USD estimates, idempotent measurements, per-meter/job totals, placement transfer estimates | User-configured rates and measurements, not a cloud invoice or automatic eBPF attribution; compute cost is excluded |
| Iceberg management | Real SQL catalog and Parquet, typed schemas, optimistic snapshot checks, append/read, bounded compaction, retained snapshot history, metadata expiration | Optional dependency; admin-only local warehouse. No standard Iceberg REST catalog, S3 warehouse configuration, distributed compaction, automatic scheduling or online orphan-file deletion |

## Postgres catalog (several replicas)

The catalog is SQLite in `--root` unless `ATLAS_DATA_DATABASE_URL=postgres://...` is set (an environment
variable, not a flag, so the password stays out of the process list). Then metadata, the audit trail and
the cost ledger live in Postgres, and write transactions are serialised across replicas by a transaction
advisory lock, so reservations, quotas and capacity checks cannot be overcommitted by two replicas racing.
Needs `pip install 'psycopg[binary]'`.

What it does not do: chunk bytes stay files under `--root`, so replicas must mount the same ReadWriteMany
volume (an Atlas filesystem works) or they will not see each other's chunks. Postgres availability and
backups are yours. Writes are serialised on one lock, which is correct but caps write concurrency; no
throughput figure is claimed.

Verified on 2026-11-01: the whole test suite passes against a throwaway Postgres 16 on the lab
(`scripts/atlas-data-postgres-remote.sh`), including a test with two `Store` replicas racing ten
reservations for one slot (exactly one wins). Not run: more than two replicas, a Postgres failover, a
shared network volume.

## Install and start

Python 3.11+; core service and tests use only the standard library. Iceberg is optional:

```sh
python3 -m venv /tmp/atlas-data-venv
. /tmp/atlas-data-venv/bin/activate
pip install -r tools/atlas_data/requirements-iceberg.txt
export PYTHONPATH=tools
export ATLAS_DATA_TOKEN="$(python3 -c 'import secrets; print(secrets.token_urlsafe(48))')"
python3 -m atlas_data token-hash
```

Store the printed SHA-256 value in a **private** configuration file. Do not use the
literal placeholder below as a real credential hash:

```json
{
  "credentials": [
    {
      "tenant": "acme",
      "subject": "operator",
      "role": "admin",
      "token_sha256": "REPLACE_WITH_TOKEN_SHA256"
    }
  ],
  "recovery_profiles": {}
}
```

```sh
chmod 600 /etc/atlas-data/config.json
python3 -m atlas_data serve --root /var/lib/atlas-data \
  --config /etc/atlas-data/config.json --capacity-bytes 107374182400
```

HTTP is accepted only on loopback. For remote nodes, supply `--host 0.0.0.0
--cert /etc/atlas-data/tls.crt --key /etc/atlas-data/tls.key`; clients use `--url
https://data.example:5120 --ca /etc/atlas-data/ca.crt`. Redirects are refused.

Tokens stay in `ATLAS_DATA_TOKEN`, not CLI arguments. The server reads hashed
credentials at startup. Remove a credential and restart to revoke it. Optional `expires_at` is a Unix
timestamp checked on every request, for short-lived agent credentials. Each
credential binds exactly one tenant, subject and role. Admin permissions apply
only inside that tenant. Use distinct subjects for agents; never give an agent
or training pod the trusted node-worker admin credential.

## Model/dataset lifecycle

```sh
python3 -m atlas_data ingest model llama model-sha ./weights
python3 -m atlas_data ingest dataset training release-001 ./dataset
python3 -m atlas_data export ARTIFACT_ID ./verified-output
```

Ingest returns an artifact ID. Files are chunked and verified against their full
SHA-256 before publication. Repeating a revision is a 409 conflict. Upload chunks
can be retried safely; a new revision references shared content instead of copying
it. Deduplication is scoped per tenant to avoid cross-tenant data discovery.

An artifact ID identifies a named immutable revision; checksums inside its manifest
identify file contents. ACLs are separate mutable records: granting/revoking readers
applies to every revision of a named model/dataset. An operator may branch an
accessible artifact under a new name, but only its owner/admin may add a revision
to the original name. Published derivatives have their own ACL; revocation does
not erase data a previously authorized reader already copied or published.

To search, write JSON arguments to a file and call the action:

```sh
python3 -m atlas_data call artifacts.list --json examples/data/search.json
```

## API and CLI actions

All actions are `POST /v1/rpc/ACTION` with a JSON object, bearer authentication,
and `Content-Length`. A mutation and its audit event commit together except Iceberg,
whose own atomic catalog commit is separate from Atlas audit persistence. A crash
between those commits may leave a completed Iceberg mutation without an Atlas audit
row. Successful replies are JSON; failures have HTTP 400/401/403/404/409/413/422/507.
Optional-component failure can return 503; unexpected library failures return 500.

| Action | Arguments |
|---|---|
| `artifacts.publish` | `kind`, `name`, `revision`, `files`, optional `readers`, `provenance` |
| `artifacts.list` | optional `query` |
| `artifacts.get` | `key` |
| `artifacts.diff` | `left`, `right` |
| `artifacts.readers` | `key`, `readers` |
| `nodes.heartbeat` | `node`, `capacity`, `bandwidth`, `labels` (admin) |
| `nodes.staged` | `node`, `artifact_id` (trusted admin worker) |
| `nodes.invalidate` | `node`, `artifact_id` (admin; immediately clears readiness after corruption/loss) |
| `nodes.evict` | `node`, `artifact_id` (admin; refuses active reservations) |
| `jobs.reserve` | `job`, `artifacts`, optional `labels`, `reserve` |
| `jobs.status` | `job` |
| `jobs.renew` | `job`, optional `ttl` (1–3600 seconds) |
| `jobs.release` | `job` |
| `workspaces.create` | `base`, `ttl` (1–86400 seconds), `budget` (changed logical bytes) |
| `workspaces.change` | `key`, `generation`, `name`, `entry` (file entry or null for delete) |
| `workspaces.review` | `key` |
| `workspaces.publish` | `key`, `generation`, `name`, `revision`, `selected` |
| `workspaces.discard` | `key` |
| `kv.put` | `compatibility`, `data` (base64), `ttl` |
| `kv.get` | `compatibility` |
| `recovery.drill` | `artifact`, `profile`, optional `rto_seconds` (1–600; admin) |
| `recovery.latest` | `name` (admin) |
| `costs.rate` | `meter`, `micros` per million meter units (admin) |
| `costs.meter` | `event`, `job`, `meter`, `units` (trusted admin collector) |
| `costs.report` | `job` (owner/admin) |
| `costs.estimate` | `artifacts` |
| `iceberg.create` | `name`, `fields` (admin) |
| `iceberg.describe` | `name` (admin) |
| `iceberg.append` | `name`, `rows`, `expected_snapshot` (admin) |
| `iceberg.rows` | `name`, optional `limit` (admin) |
| `iceberg.compact` | `name`, `expected_snapshot`, optional `max_bytes` (admin) |
| `iceberg.expire` | `name`, `older_than_seconds`, optional `retain_last` (admin) |
| `audit.list` | none (admin; last 1000 tenant events) |
| `storage.gc` | none (admin) |

Binary upload: `POST /v1/chunks`, optional `X-Content-SHA256`, <=4 MiB.
Binary retrieval: `GET /v1/chunks/HASH?artifact=ARTIFACT_ID`. The artifact ACL and
chunk membership are checked on **every request**. Knowing a content hash grants
no access. A publisher must own its uploaded chunks or read an artifact containing
them. There is no unauthenticated mode. `/healthz` contains only service health/version.

## Data-ready job lifecycle

1. Register a trusted node inventory with labels, logical cache capacity and measured
   staging bandwidth. Heartbeats expire after 120 seconds.
2. Reserve model/dataset IDs. Reservations hold capacity transactionally for 300 seconds.
   Concurrent reservations cannot oversubscribe the same node. Shared reservations
   for the same artifact consume logical capacity once.
3. Run the trusted worker on the selected node:

   ```sh
   python3 -m atlas_data stage JOB_ID NODE_ID /private/tenant-node-cache
   ```

4. The worker downloads missing chunks, verifies complete files, atomically renames
   its staging directory, then acknowledges placement. Existing files are reverified
   when staging is repeated. Cached chunks are shared across revisions on that worker.
5. `DataReadyGate.ready()` returns a Kubernetes hostname selector only while the node,
   reservation and verified placement are fresh. Placement verification leases last
   300 seconds. Re-stage to renew a verification lease. Renew long-running reservations
   and release them after the job; do not launch compute from a stale cached decision.
6. Invalidate placement before physically evicting a cache entry. Active reservations
   pin it. Trusted workers must report loss/eviction; Atlas cannot remotely prove that
   an administrator has not removed cached files after verification.

`DataReadyGate.wait_ready()` (and `python3 -m atlas_data gate-wait JOB --timeout 600 --out /gate/selection.json`)
blocks until `ready()` succeeds. Only "data not ready" is retried; a refused credential or an unknown job fails at once, and a
timeout exits non-zero, so a pod fails visibly instead of starting compute without its data. Used as an init container, it holds
the workload back until a trusted worker has staged and verified the job's artifacts:

The image is `Dockerfile.data` (non-root UID 10001, standard library plus `psycopg`; arguments choose `serve`, `gate-wait`,
`call`, `ingest`, `export`). `deploy/k8s/atlas-data.yaml` runs the service (TLS and the credential config come from
Secrets you create; none is in the file), and `deploy/k8s/atlas-data-gate-example.yaml` is a pod whose init container is
`gate-wait`, holding the workload until the data is staged:

```yaml
initContainers:
  - name: wait-for-data
    image: ghcr.io/zyvorai/atlas-data:0.10.0
    args: ["--url=https://atlas-data.zyvor-system.svc:5120", "--ca=/etc/atlas-data-ca/ca.crt", "gate-wait", "job-123", "--out=/gate/selection.json"]
    env: [{name: ATLAS_DATA_TOKEN, valueFrom: {secretKeyRef: {name: atlas-data-client, key: token}}}]
```

It waits, it does not place: the pod must already be scheduled onto the node in `selection.json` (set the
`nodeSelector`, or read the file in a submission hook). For placement of the bytes themselves, atlas-native can pin a
dataset's extents to the GPU node with `POST /v1/fs/{fs}/locality/pin` ([NATIVE_FS.md](NATIVE_FS.md)); this service does
not call it.

Verified on the lab (2026-11-01, `scripts/atlas-data-image-remote.sh`, `deploy/data/gate-smoke.sh`): the image builds;
a service container publishes a dataset, registers a node and reserves a job; `gate-wait` exits non-zero ("timed out
after 3s waiting for job job-1 data") while nothing is staged, then succeeds and writes `selection.json` naming the node
once it is staged. Both manifests pass a server-side `kubectl apply --dry-run=server` on the lab k3s. Not done: the image
is not published (the `ghcr.io/zyvorai/atlas-data` tag in the manifests is the intended name, not an existing image), no
pod has actually run on a cluster, and no Kubernetes admission or scheduler integration exists.

`tools/atlas_data/integrations.py` provides `DataReadyGate` and `CostCollector`.
The caller is responsible for matching node IDs to Kubernetes hostnames, checking
compute capacity in Gryvia, and mounting the cache into its workload. Cache capacity
uses conservative logical artifact sizes; shared physical chunks can consume less.

## Agent branches and KV cache

A workspace starts with a base manifest and empty overlay. Edits add references to
uploaded chunks; unchanged files are shared. Generation checks reject concurrent
lost updates. Time/byte limits apply to the workspace API; they do not bound agent
CPU/RAM or prevent an agent from making unrelated uploads under its own credential.
Selective publication creates a **new** dataset revision with workspace/base provenance.
Use its ID for export into a VM/container. Discard closes the overlay.

`KVAdapter(client, model, tokenizer, adapter, layout)` takes immutable compatibility
identifiers and an exact prefix digest. A miss/expired block returns `None`. Cache
keys additionally include the authenticated subject. Different models, tokenizers,
adapters, tensor layouts or subjects do not share blocks. The caller must hash the
actual token prefix; text similarity is not a valid compatibility test. Hot-cache
budget is 64 MiB per subject, capped by service capacity; disk reclamation is explicit
GC after its upload grace period, so eviction is not immediate disk reclamation.

### Prefix blocks

`prefix_digests(token_ids, block_size)` returns the chained SHA-256 digest of each full token
block, the keying prefix caches use: digest *i* covers all tokens up to block *i*, so prompts
share it only when they share that whole prefix. `KVAdapter.put_blocks(digests, blocks)` stores
one serialized block per digest and `KVAdapter.longest_prefix(digests)` returns the cached
blocks of the longest unbroken leading run, stopping at the first miss. It is engine-neutral and
stdlib-only: the serving runtime still produces and consumes the block bytes, and nothing here
says a hit makes inference faster. Unit tested with a fake client; not run against vLLM or any
engine.

## Recovery and cost evidence

A recovery profile is a server-side allowlisted argv array, for example:

```json
{
  "recovery_profiles": {
    "orders": {
      "verify": ["/opt/atlas/verify-orders", "{workspace}"]
    }
  }
}
```

The trusted executable must start/check/stop its own application inside the supplied
restored directory, and exit zero only on success. No commands or paths are accepted
from the remote request. Processes get a minimal environment without API tokens,
no shell, a deadline and process-group cleanup. Hooks are trusted code running as the
service account; use external VM/container isolation if the application is untrusted.
The result separates checksum verification from application verification. It does not
assert that an artifact is malware-free. For live Native/Ceph volume capture and
restore, retain the existing [application recovery tool](APPLICATION_RECOVERY.md).

Supported cost meters are `stored_byte_hours`, `transfer_bytes`, `cache_miss_bytes`
and `checkpoint_bytes`. Rates are micro-USD per million units, rounded upward per
event. Event IDs are immutable/idempotent; retrying after a rate change preserves
the original amount. Only trusted collectors create entries. Set rates explicitly;
unknown rates fail rather than pretending storage is free. Placement estimates
include data transfer only, excluding compute time, taxes and cloud request charges.

## Iceberg

Install the optional requirements before creating tables. Types: `long`, `double`,
`string`, `boolean`. Appends require the expected current snapshot ID (`null` for
an empty table). Compaction uses an atomic Iceberg overwrite, preserving old snapshots
for time travel; stale snapshot IDs fail with 409. It is bounded to 128 MiB compressed
input and one million rows, then checks decoded Arrow size. Decoding can transiently
exceed that target for highly compressed data: enforce a process/container memory
limit. Use an external engine for large tables.

Snapshot expiration retains at least two newest snapshots and all named references.
It removes metadata references only; physical orphan deletion is excluded to avoid
removing data needed by in-flight readers. The warehouse is private and its storage
is **not included** in the CAS byte cap: enforce a filesystem/PVC quota. Catalog paths
are tenant-derived, never request-supplied. No arbitrary SQL or warehouse URL is accepted.

## Operations and verification

```sh
make data-test
# Optional dependency required for the Iceberg test and all-nine demo:
pip install -r tools/atlas_data/requirements-iceberg.txt
make data-demo
```

The all-nine demo uses real HTTP, local model/dataset bytes, node staging, a workspace,
KV serialization, a restored SQLite application check, cost measurements, and actual
Iceberg compaction. It does not call a GPU or a remote cluster.

Run exactly **one service process per root**; SQLite metadata is not an HA/multi-process
service. HTTP requests are bounded to 32 concurrent threads, 8 MiB JSON and 4 MiB
chunks. Readers, file manifests and compaction also need deployment resource limits.
The SQLite root, worker cache and trusted profiles must not be writable by agents.
Back up metadata, chunks and Iceberg catalog/warehouse together while the service is
stopped. Restore into a new private root and verify artifact checksums before use.
The root should be a persistent volume supplied by Atlas/Ceph/ZFS/NFS; see
[deployment example](../deploy/data/README.md).

GC reclaims unreferenced tenant chunks after a one-hour grace period. Published
artifacts remain retained; no online artifact deletion is supplied. Expired workspace
and KV metadata is retained for inspection. Orphan files from a crash between disk
write and catalog commit may need offline cleanup; GC must not be used as a retention
compliance mechanism. Audit metadata is operational evidence, not a tamper-proof log.
