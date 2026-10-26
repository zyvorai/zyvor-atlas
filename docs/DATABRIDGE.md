<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Zyvor DataBridge — cloud-to-edge database mobility

DataBridge migrates **managed cloud databases** to **open, self-managed engines at the edge** on
Kubernetes, with **Ceph (via Atlas/Rook) as the storage layer**. It runs from Atlas as a migration
control plane, reusing the job engine, inventory, gateway, and Zeus OS console. Ceph is the *storage*,
not the database engine.

**Supported source engines** (`kind`):

| Source engine | Aliases | Edge target | Migration | Real connector |
|---|---|---|---|---|
| PostgreSQL | `postgres`, `postgresql`, `pg` | CloudNativePG (Postgres) | homogeneous | `tokio-postgres` (default) |
| MySQL | `mysql` | Percona XtraDB (MySQL) | homogeneous | `sqlx` (default) |
| MariaDB | `mariadb`, `maria` | Percona XtraDB (MySQL) | homogeneous | `sqlx` (default) |
| Oracle | `oracle`, `ora` | CloudNativePG (Postgres) | **heterogeneous** | `oracle`/OCI (feature `oracle`) |
| SQL Server | `sqlserver`, `mssql`, `sql-server` | CloudNativePG (Postgres) | **heterogeneous** | `tiberius` (feature `sqlserver`) |
| MongoDB | `mongodb`, `mongo` | Percona Server for MongoDB (**document**) | homogeneous | `mongodb` driver (feature `mongodb`) |

*Homogeneous* migrations (Postgres/MySQL/MariaDB/MongoDB) copy the data with a `dump→restore` full-load
Job, then Debezium streams changes (`snapshot.mode=no_data`: schema only, no data snapshot). *Heterogeneous* migrations (Oracle / SQL
Server → Postgres) have **no dump full-load**: Debezium's `initial` snapshot seeds the edge and the JDBC
sink creates the tables, then it streams (the standard cross-engine pattern). **MongoDB** is the
one document engine — it lands on a Percona Server for MongoDB replica set (`rs0`, required for change
streams), full-loads with `mongodump | mongorestore`, and uses the **MongoDB Kafka sink** (with the
Debezium Mongo CDC handler) instead of the JDBC sink. Every engine runs the whole pipeline
**fake-first** with no cloud creds; the real Oracle / SQL Server / MongoDB connectors are behind cargo
features because they link a native client (OCI) / TLS stack / driver.

An **Oracle** source's `database` is `CDB/PDB` for a multitenant database (e.g. `FREE/FREEPDB1`,
`ORCLCDB/ORCLPDB1`): discovery connects to the PDB service, while Debezium's LogMiner connects to
the CDB and captures the PDB. A bare name means a non-CDB database.

> Positioning: *"Migrate managed cloud databases (RDS/Aurora, Cloud SQL, Azure SQL, on-prem Oracle)
> to open engines running on Zyvor Edge, backed by Ceph — with continuous replication, validation,
> cutover, and rollback from one control plane."*
> DynamoDB / Firestore / Spanner are **out of scope** — those are data-model migrations, not storage
> migrations.

## Pipeline
```
Discover → Assess → Provision (edge DB on Ceph) → Full-load → CDC (Debezium)
        → Validate → Cutover → (Rollback within window)
```
Each stage is an async job (`202 + job id`, progress via `/jobs/{id}/watch`). Long-running work
(edge CR readiness, CDC lag) is advanced by the **DataBridge reconciler** worker.

- **Edge runtime**: CloudNativePG (Postgres) / Percona XtraDB (MySQL) / Percona Server for MongoDB,
  data + WAL on the `zyvor-rbd-prod` Ceph RBD StorageClass.
- **CDC**: Debezium on Strimzi/Kafka (cloud-neutral, reads WAL/binlog/redo/oplog — Postgres, MySQL,
  MariaDB, Oracle LogMiner, SQL Server, MongoDB change streams) → Debezium JDBC sink (relational) or
  the MongoDB Kafka sink (document) → edge DB.
- **Cutover** is admin-guarded: plan `validated` + last validation `passed` + CDC `lag_seconds` under
  threshold; **rollback** only within the plan's rollback window. In real mode the cutover job leaves
  the record `draining`; the reconciler completes the switch once the CDC stream reports **zero** lag
  (tearing down the Debezium source/sink connectors + the per-plan KafkaConnect cluster), or fails the
  cutover and returns the plan to `validated` if the lag hasn't drained by the 5-minute drain deadline.
  Stopping CDC (`cdc/stop`) in real mode deletes the two `KafkaConnector` CRs (the KafkaConnect cluster
  stays up so a later start/restart re-instantiates quickly).

## Fake vs real
The whole pipeline runs **fake-first** with no cloud creds or operators — connectors serve a canned
schema, edge provisioning/full-load/CDC/validate complete inline, and the reconciler drains a
synthetic CDC lag so the UI animates. Set a source's `driver_mode: real` (creds in a k8s Secret) and
install the edge operators to run it for real.

## Run locally (fake, no cloud/k8s)
```bash
make run-databridge          # gateway with fake Ceph driver + DataBridge reconciler @5s
```
Then open the console at http://127.0.0.1:5110/ → **DataBridge → Cloud Databases**, register a
source (kind `postgres`/`mysql`/`mariadb`/`oracle`/`sqlserver`, driver mode `fake`), and walk the plan
through the stepper. Or over REST:
```bash
B=http://127.0.0.1:5110/api/atlas/v1
SID=$(curl -sX POST $B/databridge/sources -d '{"name":"orders","kind":"postgres","cloud":"rds"}' | jq -r .id)
curl -sX POST $B/databridge/sources/$SID/discover
PID=$(curl -sX POST $B/databridge/plans -d "{\"name\":\"orders\",\"source_id\":\"$SID\"}" | jq -r .id)
for stage in assess provision full-load cdc/start validate cutover; do curl -sX POST $B/databridge/plans/$PID/$stage; done
```

## Run for real (edge cluster)
1. Install the edge operators: [`deploy/databridge/up.sh`](../deploy/databridge/README.md)
   (CloudNativePG + Percona + Strimzi) — needs the `zyvor-rbd-prod` StorageClass.
2. Register a source with `driver_mode: real` and `secret_ref`/`secret_namespace` pointing at a k8s
   Secret holding the source credentials (never sent in the API).
3. Run the pipeline; the edge `Cluster` CR binds PVCs on Ceph, the reconciler polls it to `ready`.

## REST endpoints (`/api/atlas/v1`)
| Method | Path | Purpose |
|---|---|---|
| GET/POST | `/databridge/sources` | list / register sources |
| GET/DELETE | `/databridge/sources/{id}` | get / delete a source |
| POST | `/databridge/sources/{id}/discover` | discover schema (job) |
| GET/POST | `/databridge/plans` | list / create plans |
| GET | `/databridge/plans/{id}` | plan detail |
| POST | `/databridge/plans/{id}/assess` | score readiness (job) |
| POST | `/databridge/plans/{id}/provision` | provision edge DB (job) |
| POST | `/databridge/plans/{id}/full-load` | full-load (job) |
| POST | `/databridge/plans/{id}/cdc/start` · `/cdc/stop` · `/cdc/restart` | CDC control + self-heal (job) |
| POST | `/databridge/plans/{id}/validate` | validate source vs edge (job) |
| POST | `/databridge/plans/{id}/cutover` | cutover — **admin, guarded** (job) |
| POST | `/databridge/plans/{id}/rollback` | rollback within window — **admin** (job) |
| GET | `/databridge/{edge-clusters,cdc-streams,validations,cutovers}` | read models |

## Object-storage migration (the object leg)

Alongside the database pipeline, DataBridge moves **object storage** — AI datasets, model
weights, checkpoints, RAG source documents, embeddings exports — from a cloud object store
into a **local S3-compatible bucket** (Ceph RGW by default; any other S3-compatible destination
also supported — `dest_endpoint`/`dest_bucket` are always caller-specified, there is no implicit
destination), so Forge/Zeus consume data locally instead of the cloud. Implemented in
`atlas-databridge::object` on top of `atlas-driver-rgw::S3Target` (SigV4, path-style, streaming
sha256 — a generic S3 client, not Ceph-specific despite the crate name), driven by the same job
engine + `/jobs/{id}/watch` SSE.

The copy is: **list source → diff vs destination (full, or incremental by key+size) →
stream each changed object recording its sha256 → verify every planned key landed at the
right size**. Credentials are never stored — the record holds only a reference to a k8s
Secret (`{access_key, secret_key}` or `{AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY}`),
resolved in-process at run time and never logged.

**Verified live** on the real Rook Ceph lab: a self-migration through the gateway's own RGW
(one bucket as `source`, another as `dest`, both on the same cluster) driven through
`create → start → copying → verifying → completed`, confirming objects are actually copied
byte-for-byte (not just an empty-bucket smoke test) with `objects_done`/`bytes_done` and
per-object sha256 verification matching source and destination.

### Multi-cloud providers
The mover speaks the **S3 protocol**, so it covers any S3-compatible source today. Non-S3
clouds are recognized in the model so the API is multi-cloud from day one, but need their own
connector (same philosophy as the DB side gating oracle/mongodb behind features):

| `source_provider` / `dest_provider` | Status | How |
|---|---|---|
| `aws` (AWS S3) | ✅ works | S3 protocol, SigV4 |
| `gcs` (Google Cloud Storage) | ✅ works | GCS **S3-interoperability** endpoint + HMAC keys |
| `s3-compatible` (MinIO, Wasabi, DO Spaces, Ceph RGW, Garage, …) | ✅ works | S3 protocol |
| `azure-blob` (Azure Blob Storage) | ✅ native connector (feature `azure-blob`) | pure-Rust Azure SDK; not S3-native, so it implements `ObjectSource` directly |
| `vmware` (vSphere/vSAN datastores) | ❌ not an object store | VMs/VMDKs live on block storage — migrate via the block (RBD import) leg, not object copy |

The source is pluggable via the `ObjectSource` trait (list + get→sha256); the destination is any
S3-protocol endpoint the caller names via `dest_endpoint` (the only check is
`dest_provider.is_s3_protocol()`). `azure-blob` is a native, feature-gated connector (below);
`vmware` is rejected with guidance to use the volume path; any other non-S3 source errors clearly
rather than failing silently.

### Object migration in the console

**DataBridge → Object Migrations** (`/databridge/object`). Both `source_endpoint`/`source_secret_ref`
and `dest_endpoint`/`dest_secret_ref` are always caller-specified — there is no backend-id shortcut
that auto-fills them (a prior RustFS-specific convenience along those lines was retired along with
the first-party RustFS integration, see `docs/RUSTFS.md`; `source_backend_id`/`dest_backend_id` on
the request are rejected with a clear error if set). The destination bucket must already exist
(create it on the Buckets page). Verified live: an RGW-to-RGW self-migration on the same cluster,
`completed`, objects copied byte-for-byte with per-object sha256 verification.

### Object REST endpoints (`/api/atlas/v1`, `require_role(operator)`, tenant-scoped)
| Method | Path | Purpose |
|---|---|---|
| GET/POST | `/databridge/object` | list / create an object migration (create is synchronous — no copy yet) |
| GET/DELETE | `/databridge/object/{id}` | status (counts, bytes, state, verified) / delete |
| POST | `/databridge/object/{id}/start` | enqueue the copy job (202 + job id); stream via `/jobs/{id}/watch` |

State machine: `created → planning → copying → verifying → completed | failed`, with
`objects_{total,done}` / `bytes_{total,done}` / `verified` updated as the job runs.

## Configuration
| Env | Default | Meaning |
|---|---|---|
| `ATLAS_DATABRIDGE_RECONCILE_SECS` | `15` | reconciler tick (edge CR readiness, full-load/validation Jobs, CDC health). `0` disables. |
| `ATLAS_DATABRIDGE_CONNECT_IMAGE` | *(unset)* | Kafka Connect image bundling Debezium + a JDBC-sink plugin (see `deploy/databridge/connect/Dockerfile`). Required for real CDC. |

Source `driver_mode` (`fake`/`real`) is per-source, set at registration. Edge namespace is
`zyvor-databridge`; edge storage class is `zyvor-rbd-prod`. Real CDC also expects a Strimzi Kafka named
`zyvor-kafka` in the edge namespace (bootstrap `zyvor-kafka-kafka-bootstrap:9092`).

### Cargo features (`atlas-databridge`)
The default build supports **fake mode for all engines** and **real Postgres / MySQL / MariaDB** (no
extra system libraries). The real connectors that need a native dependency, and precise CDC lag, are
behind features so a missing lib never breaks the default build:

| Feature | Enables | Native dependency |
|---|---|---|
| *(default)* | fake all engines + real Postgres (`tokio-postgres`), MySQL/MariaDB (`sqlx`) | none |
| `sqlserver` | real SQL Server source connector (`tiberius`, TDS) | a TLS backend (rustls, bundled) |
| `oracle` | real Oracle source connector (`oracle`/ODPI-C) | Oracle Instant Client at runtime |
| `mongodb` | real MongoDB source connector (`mongodb` async driver) | none (bundled bson + TLS) |
| `kafka-lag` | precise CDC offset-lag (embedded Kafka client) in the reconciler | `librdkafka` via `cmake` |
| `azure-blob` | native Azure Blob Storage object-migration **source** (`azure_storage_blobs`) | none (pure-Rust Azure SDK + TLS) |

Build the gateway with, e.g., `cargo build -p atlas-gateway --features atlas-databridge/sqlserver,atlas-databridge/oracle,atlas-databridge/mongodb,atlas-databridge/kafka-lag`.

## Source TLS (Postgres)

A source's `tls_mode` (default `require` through the API) decides how Postgres discovery connects, using
the libpq `sslmode` names:

| `tls_mode` | Behaviour |
|---|---|
| `disable` | Plaintext. |
| `prefer` | TLS when the server offers it, otherwise plaintext. The certificate is not verified. |
| `require` | TLS is mandatory. The certificate is not verified. |
| `verify-ca`, `verify-full` | TLS is mandatory and the certificate chain and host name are verified. `verify-ca` is treated as `verify-full`. |

An unrecognised value is treated as `require`, so a typo cannot downgrade a connection to plaintext. The
`verify-*` modes trust the platform root store plus, optionally, a PEM bundle named by
`ATLAS_DATABRIDGE_PG_CA_FILE` (for a private CA). The same `tls_mode` is passed to the full-load and
validation jobs as `sslmode`.

Checked on 2026-10-26 against a Postgres 14 that accepts only `hostssl` connections, with a private CA and a
server certificate for `127.0.0.1`: `disable` was refused by the server; `prefer` and `require` connected;
`verify-full` failed without the CA and connected with it; an unknown mode connected over TLS. A full load and
CDC run over TLS has not been done.

## Testing
- **Fake pipeline (no infra)**: `cargo test --workspace` covers the unit tests plus the gateway
  integration suites — `tests/databridge_pipeline.rs` (Postgres state machine + cutover guard) and
  `tests/databridge_engines.rs` (all six engines discover→validate + correct edge routing:
  cnpg/percona/psmdb).
- **Real source connectors (containers)**: each engine has an env-gated `#[tokio::test]` that runs only
  when its `DATABRIDGE_TEST_*` var is set (skips otherwise), mirroring `discovers_real_postgres`:

  | Engine | Var (value: `host,port,database,user,password`) | Feature |
  |---|---|---|
  | Postgres | `DATABRIDGE_TEST_PG` (libpq conn string); `DATABRIDGE_TEST_PG_TLS` picks the `tls_mode` (default `disable`) | *(default)* |
  | MySQL | `DATABRIDGE_TEST_MYSQL` | *(default)* |
  | MariaDB | `DATABRIDGE_TEST_MARIADB` | *(default)* |
  | MongoDB | `DATABRIDGE_TEST_MONGO` | `mongodb` |
  | SQL Server | `DATABRIDGE_TEST_MSSQL` | `sqlserver` |

  **`scripts/test-connectors.sh`** automates this: it spins each engine as an ephemeral podman/docker
  container, seeds a `customers`/`orders` schema, exports the var, runs the gated test, and tears the
  container down (`scripts/test-connectors.sh [pg mysql mariadb mongo mssql]`). Oracle is compile-only
  (needs the OCI client). **`scripts/test-all.sh`** runs the whole gate (clippy + workspace tests +
  feature compiles for sqlserver/mongodb/azure-blob/oracle[/kafka-lag] + optional UI build + the
  container connector tests). CI additionally builds the Docker UI stages and the full
  `Dockerfile.ceph` image.

## Data model
`migration_sources`, `migration_plans`, `edge_db_clusters`, `cdc_streams`, `validation_runs`,
`cutovers` (SQLite; migration `0011_databridge.sql`). Plan state machine:
`draft → discovered → assessed → provisioning → provisioned → full_loading → loaded →
cdc_streaming → validating → validated → cutover_pending → cutover_in_progress → cutover_complete
→ completed | rolled_back | failed`.

## Live verification (real infra)
**Verified end-to-end on two independent live Rook Ceph clusters** (`<ephemeral-ip>`, `<ephemeral-ip>`).
A full real migration was driven through the deployed gateway (k3s + Rook Ceph), Postgres →
Ceph-backed edge Postgres:

| Stage | Result |
|---|---|
| discover | gateway's `tokio-postgres` connector introspected a live Postgres **16.14** (tables, PKs, sizes, `wal_level=logical`) |
| assess | readiness **90/100** (flagged the no-PK `audit_log`) |
| provision | gateway applied a **CloudNativePG `Cluster` CR**; data + WAL PVCs **Bound on `zyvor-rbd-prod`**; reconciler advanced `provisioning → provisioned` when it reached healthy |
| full-load | gateway ran a **`pg_dump\|psql` batch Job**; **data really copied** (100 customers, 500 orders verified in the edge DB) |
| validate | gateway ran a **row-count-compare Job**; reconciler → `validated` |
| cutover | guarded (validated + validation passed) → endpoint switched source → `edge-…-rw.zyvor-databridge.svc:5432`, 72h rollback window |
| rollback | within window → `rolled_back` |

Prereqs proven in the process: `deploy/databridge/up.sh --pg-only` installs CloudNativePG on the real
cluster; the gateway's **write RBAC** (CNPG `clusters`, batch `jobs`, Strimzi connectors, `secrets`)
is sufficient; the CNPG `<cluster>-app` Secret exposes the `uri`/`password` keys the loader uses.

**CDC (Strimzi/Debezium) — VERIFIED end-to-end live.** On the Rook cluster: Strimzi + a KRaft Kafka +
a custom Connect image (Debezium PostgreSQL + Aiven JDBC sink, `deploy/databridge/connect/Dockerfile`,
set via `ATLAS_DATABRIDGE_CONNECT_IMAGE`). The gateway's `cdc/start` created the KafkaConnect + Debezium
source + JDBC-sink connectors; a row inserted into the source Postgres **replicated to the Ceph-backed
edge Postgres** (source → Debezium → Kafka → JDBC sink → edge), and continuous inserts converged.

Getting there corrected **ten real issues** in the blind-built CDC path (all now in the code / deploy),
confirmed by re-running the whole flow on the **second** cluster: `cdc/start` there produced both
connectors RUNNING and replicated a row to the edge with **zero manual patching** — the committed code
+ `deploy/databridge/` bundle stand up working CDC on their own.
1. Strimzi serves the kinds at `kafka.strimzi.io/v1` (not `v1beta2`);
2. bootstrap Service is `<kafka>-kafka-bootstrap:9092`;
3. `KafkaConnect` needs top-level `groupId` + `*StorageTopic` (not under `config`);
4. Connect image must match the operator's Kafka major (4.x base) + Debezium 3.x;
5. single-broker lab needs `{config,offset,status}.storage.replication.factor: 1`;
6. `KafkaConnect` needs `strimzi.io/use-connector-resources: "true"` or connectors are ignored;
7. the Connect ServiceAccount needs RBAC to `get` Secrets (`deploy/databridge/connect-rbac.yaml`);
8. the JDBC sink needs the Debezium `ExtractNewRecordState` unwrap SMT + a `RegexRouter` (topic→table)
   + `pk.mode=record_key`/`pk.fields`;
9. Debezium must use `decimal.handling.mode=double` (Postgres unscaled `numeric` → a STRUCT the sink
   can't bind) + `snapshot.mode=never` (the full-load already seeded the edge);
10. the JDBC sink needs `consumer.override.metadata.max.age.ms` shortened (default 5 min) so it discovers
    a table's Debezium topic (created on the first change) promptly, without a restart.

Real CDC is **health-tracked** by the reconciler: a streaming stream backed by a real Debezium
`KafkaConnector` follows the connector's state (RUNNING → live, else → `error`); fake
streams keep the synthesized lag drain. The JDBC sink upserts on every record-key column of each
table, so no per-deployment primary-key setting is needed. With the **`kafka-lag`** feature, a healthy real
stream also reports its **precise offset-lag** (∑ per-partition `high_watermark − committed_offset` of
the sink consumer group `connect-<sink-connector>`) via an embedded Kafka client; without it, a healthy
stream reports caught-up (0).

## Status
- **Done + verified end-to-end on two live Rook Ceph clusters** (and CI-locked by
  `tests/databridge_pipeline.rs` for the fake path): the entire pipeline — discover (real
  `tokio-postgres` connector) → assess → provision (CNPG/Percona CR on Ceph, reconciler-advanced) →
  full-load (`pg_dump|psql` / `mysqldump|mysql` batch Jobs, reconciler-watched) → **CDC** (Strimzi
  `KafkaConnect` + Debezium source + JDBC-sink connectors, live source→edge replication) → validate
  (row-count-compare Job) → cutover (guarded) → rollback.
- Real CDC is **health-tracked** by the reconciler; the JDBC sink keys on each table's own primary key.
- **Engine coverage**: all six source engines (Postgres, MySQL, MariaDB, Oracle, SQL Server, MongoDB)
  run the full pipeline; the fake path for every engine is exercised end-to-end (discover→…→cutover).
  Real Postgres/MySQL/MariaDB connectors are default; real SQL Server (`tiberius`), Oracle (`oracle`/OCI)
  and MongoDB (`mongodb` driver) are behind cargo features; precise CDC lag is behind `kafka-lag`.
  Oracle/SQL Server are heterogeneous (→ Postgres edge, seeded by the Debezium initial snapshot);
  MongoDB is a homogeneous document migration (→ Percona Server for MongoDB, Mongo Kafka sink).
- **Day-2 self-heal**: a stalled/errored CDC stream is re-established by `POST .../cdc/restart` (bumps
  the stream's `restart_count` and re-applies the connector CRs); the reconciler also **auto-restarts**
  an unhealthy stream up to 3 times before giving up (→ `error`, which the monitor's CDC-error rule then
  alerts on).
- **MySQL / MariaDB / MongoDB / SQL Server real-connector discovery — verified live** (2026-07): real
  connector discovery run against a real MySQL 8.4, MariaDB 11.x, MongoDB 7.0 (single-member replica set),
  and SQL Server 2022 (CDC-enabled) on the deployed k3s gateway — engine/version, `cdc_capable` (binlog
  ROW for MySQL/MariaDB; replica-set change streams for Mongo; `sys.databases.is_cdc_enabled` for SQL
  Server), real database names, and real per-table/collection schema+name with PK / document counts all
  returned correctly. The MySQL/MariaDB run surfaced and fixed a real bug: sqlx-mysql won't decode
  `information_schema` string columns (binary-ish collation) as `String`, so the names came back empty
  until the queries were changed to `CAST(... AS CHAR)` (the fake tests couldn't catch it — canned data).
- **Oracle real-connector discovery — verified live** (2026-07): the gateway image now bundles the OCI
  Instant Client (Basic Lite) and builds the `oracle` feature; discovery against a real Oracle 23ai/26ai
  Free (`gvenzl/oracle-free`) returned engine/version, supplemental-log-min `cdc_capable`, real user
  schemas, and the real user tables with PKs. **All six source engines are now discovery-verified live.**
- **System-schema filtering — fixed (surfaced live)**: real Oracle/SQL Server discovery leaked internal
  objects (Oracle 23ai/26ai `VECSYS`/`DBSFWUSER`/`BAASSYS`/…; SQL Server `cdc.*` + `dbo.systranschemas`)
  as migratable tables. Fixed to use each engine's own metadata flag — Oracle `all_users.oracle_maintained
  = 'N'` and SQL Server `sys.tables.is_ms_shipped = 0` (+ excluding the `cdc`/`sys` schemas) — instead of
  a fragile hardcoded denylist.
- **MySQL + MariaDB full-load + validate — verified live** (2026-07): drove the real gateway pipeline
  register → discover → assess → provision → full-load → validate against a lightweight edge (standing in
  for the Percona edge: the gateway created the real `PerconaXtraDBCluster` CR, and the reconciler
  advanced the edge to `ready` off the CR's status). The **full-load ran the real `mysqldump→mysql` batch
  Job and physically copied the data** source→edge, and the **validate row-count-compare Job passed**
  (plan → `validated`). The MariaDB run surfaced + fixed a real loader bug: the MySQL-8 `mysqldump` client
  fails on a MariaDB source (`Unknown table 'COLUMN_STATISTICS'`, 1109) unless `--column-statistics=0` is
  passed — now added (harmless for MySQL). (Streaming CDC + cutover for MariaDB landed later —
  see the 2026-09-01 entry below.)
- **MongoDB full-load + validate — verified live** (2026-07): drove provision → full-load → validate
  against a lightweight edge PSMDB replica set. Surfaced + fixed **two real bugs**: (1) the gateway
  ClusterRole was missing `psmdb.percona.com/perconaservermongodbs`, so MongoDB edge provisioning was
  RBAC-forbidden (added to `deploy/k8s/atlas-gateway.yaml`); (2) the `mongodump`/`mongorestore`/`mongosh`
  URIs in the loader + validation Jobs omitted `authSource=admin`, so authenticated Mongo sources failed
  SCRAM auth (the mongo tools default the auth db to the app db, not `admin`). The **full-load copied the
  data** (3 customers + 1 order) and **validate confirmed exact document-count parity** (plan → `validated`).
  (CDC + cutover landed 2026-09-01 — see below.)
- **MariaDB real CDC + cutover — verified live** (2026-09-01, `212.8.248.187`): Connect image with
  `debezium-connector-mariadb`, MariaDB 11 lab source (`utf8mb4_unicode_ci` — MariaDB 11's default
  `utf8mb4_uca1400_ai_ci` is rejected by Percona/MySQL edge restore), plan through full-load → CDC
  (KafkaConnect + MariaDbConnector + JDBC sink Ready) → validate → cutover_complete
  (`edge-…-haproxy.zyvor-databridge.svc:3306`).
- **MongoDB real CDC + cutover — verified live** (2026-09-01, `212.8.248.187`): gateway rebuilt with
  `atlas-databridge/mongodb` (+ kafka-lag), PSMDB edge, `mongodump|mongorestore` full-load, Debezium
  Mongo source (`snapshot.mode=no_data` — 3.x rejects `never`) + Mongo Kafka sink, validate →
  cutover_complete. (Its sink routing and CDC handler were replaced on 2026-10-03, see below.)
  Also corrected PSMDB users Secret name to `internal-<cluster>-users` (operator 1.16+).
- **MySQL real CDC — verified live end-to-end for DATETIME columns** (2026-08-25/26,
  `<ephemeral-ip>`): full-load re-verified end-to-end against a real Percona edge (3 customer
  rows landed, confirmed via the API-driven job), then `cdc/start` applied a real `KafkaConnect`
  cluster + Debezium MySQL source connector + Aiven JDBC sink connector against a real MySQL 8.4
  source. Surfaced and fixed **eight real bugs** in the code: (1) `mysql_operator.rs`'s PXC CR
  builder never set `allowUnsafeConfigurations`, so Percona's operator never reports a
  single-node edge `ready`; (2)-(4) `loader.rs`'s `mysqldump` pipeline needed `CREATE DATABASE`
  first, `--no-tablespaces` (the source creds lack `PROCESS`), and `--skip-add-locks` (PXC's
  `pxc_strict_mode` rejects `LOCK TABLES`); (5) `loader.rs` also needed `--set-gtid-purged=OFF`
  (the edge's own non-empty `GTID_EXECUTED` conflicts with the source's); (6) `streaming.rs`'s
  `connect_spec()` used Strimzi's default liveness/readiness probe (~90s to first kill), too
  tight for a Connect image bundling Debezium + JDBC plugins — extended to 180s/6 retries; (7)
  `streaming.rs`'s Debezium source config was missing `time.precision.mode: connect` (needed for
  `DATETIME`/`TIME`/`DATE` columns — see the TIMESTAMP caveat below); (8)
  `deploy/databridge/connect/Dockerfile` pinned Debezium 3.0.8.Final against a Kafka 4.3.0 base
  image, but 3.0.8's schema-history recovery calls a Kafka client method Kafka 4.0 removed
  (`NoSuchMethodError`) — bumped to 3.3.0.Final.

  Two further operational lessons surfaced getting a row to actually land on the edge, neither
  of which is a code bug:
  - **A sink task's Kafka consumer offset gets permanently poisoned by any failed message.**
    Kafka consumers process a partition strictly in order — once a message fails and the task
    dies, restarting the task (`strimzi.io/restart-task`) resumes from the *same* committed
    offset and re-fails on the *same* message forever, silently blocking every later row too
    (the connector/task status can report `RUNNING` the whole time, which is misleading — check
    `kafka-consumer-groups.sh --describe` for `LAG` to see the truth). The fix is to explicitly
    advance the consumer group's offset past the poisoned message:
    `kafka-consumer-groups.sh --bootstrap-server <broker> --group connect-<sink-name> --topic
    <topic> --reset-offsets --to-offset <N>|--to-latest --execute`.
  - **MySQL `TIMESTAMP` columns are not `DATETIME` columns, and Debezium treats them very
    differently.** Debezium's MySQL connector always encodes a `TIMESTAMP` column as an
    ISO-8601 string via its own `io.debezium.time.ZonedTimestamp` logical type — this is
    unconditional, unaffected by `time.precision.mode` (which only governs `DATETIME`/`TIME`/
    `DATE`). The Aiven JDBC sink doesn't understand that Debezium-specific logical type and
    fails trying to bind the raw ISO string into a MySQL column
    (`MysqlDataTruncation: Incorrect datetime value: '2026-08-25T19:12:56Z'`), permanently
    killing the sink task on every row that carries a `TIMESTAMP` value. This is a genuine,
    **still-open product gap**: `created_at`/`updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP`
    is an extremely common MySQL pattern, so real customer tables will hit this. Verification
    here altered the throwaway test table's column from `TIMESTAMP` to `DATETIME` to get a
    clean signal — that is **not** a fix, just a workaround for this session. A real fix needs
    either a Kafka Connect SMT that converts `io.debezium.time.ZonedTimestamp` fields to a
    JDBC-bindable type before the sink writes them, or documented customer guidance to avoid
    `TIMESTAMP` columns (prefer `DATETIME`) on tables enrolled in CDC until that SMT exists.
    **Fixed (2026-10-03):** MySQL-family edges now use the **Debezium JDBC sink**
    (`io.debezium.connector.jdbc.JdbcSinkConnector`, bundled in the Connect image), which reads
    the Debezium envelope and binds `ZonedTimestamp` natively; it also takes every record-key
    column as the primary key (composite keys work without `pk.fields`) and applies deletes.
    Verified on `212.8.248.187` with the exact connector configs DataBridge generates (Debezium
    source + Debezium JDBC sink in the `deploy/databridge/connect` image, KRaft Kafka, MySQL 8.4
    edge, all in podman): for both a **MySQL 8.4** and a **MariaDB 11.4** source, `TIMESTAMP`
    columns (`DEFAULT CURRENT_TIMESTAMP` and explicit values), `DATETIME`, an update, a delete
    and a composite-key table all landed on the edge identical to the source. Postgres edges
    moved to the same sink afterwards (next item).
- **Every engine on Debezium 3.7 + the Debezium JDBC sink — verified (2026-10-03,
  `212.8.248.187`)**, with the exact connector configs DataBridge generates, in standalone podman
  (KRaft Kafka + the `deploy/databridge/connect` image + real source and edge databases; the shared
  k3s cluster was left alone). Each run seeded rows, then applied an insert, an update and a delete
  on the source and compared source and edge:
  - **SQL Server 2022 → Postgres 16** and **Oracle 26ai Free → Postgres 16** (heterogeneous): the
    Debezium `initial` snapshot created and seeded the edge table, then the changes streamed.
  - **Postgres 16 → Postgres 16**, **MySQL 8.4 / MariaDB 11.4 → MySQL 8.4** (homogeneous, edge
    pre-seeded like the full-load leaves it, `snapshot.mode=no_data`), including composite keys
    and MySQL `TIMESTAMP` columns.
  - **MongoDB 7 → MongoDB 7** (replica sets, `no_data`).

  What it took:
  1. **Debezium 3.3 → 3.7** in the Connect image. 3.3's Oracle connector looks the version up with
     `BANNER_FULL LIKE 'Oracle Database%'`; the 26ai banner reads "Oracle AI Database 26ai", so
     validation failed with `Failed to resolve Oracle database version`. 3.7 reads it from JDBC
     metadata. 3.7 also **removed `snapshot.mode=never`**, so homogeneous relational sources now
     use `no_data` (schema only), as MongoDB already did.
  2. **Postgres edges use the Debezium JDBC sink** too. The Aiven sink needs one `pk.fields` for
     every table of a connector (it rejects an empty value) and, as configured, left deleted rows on
     the edge; the Debezium sink
     keys each table on its own record key, applies deletes, and creates the heterogeneous edge
     tables (`schema.evolution=basic`). `ATLAS_DATABRIDGE_SINK_PK_FIELDS` is gone.
  3. **Topics route to their last segment**: SQL Server topics are `<prefix>.<db>.<schema>.<table>`,
     and keeping everything after the prefix's first segment made rows target a `dbo.customers`
     table (`schema "dbo" does not exist`).
  4. **Oracle sources name the container database**: Debezium connects to the CDB
     (`database.dbname`) and captures the PDB (`database.pdb.name`); the source's `database` is
     now `CDB/PDB` (e.g. `FREE/FREEPDB1`), a bare name meaning a non-CDB. It used to send the PDB
     as both.
  5. **Mongo sink**: the `ChangeStreamHandler` (DataBridge captures change streams; the
     `MongoDbHandler` it used is for the removed oplog mode and failed every update with "Update
     document missing `patch` field"), and the target collection now comes from each event's
     `source.collection` instead of a RegexRouter, whose bare-name `topics.regex` alternative also
     subscribed Connect's own config/offset/status topics and killed the task.

  With the column altered to `DATETIME` and the poisoned offset advanced, a fresh row
  (`'Jack CDC Datetime Fixed'`) was confirmed **physically present on the edge PXC**, proving
  the full pipeline (Debezium source → Kafka → JDBC sink → edge write) genuinely works. `validate`
  was then run for real and correctly reported a row-count **mismatch** (source 11 rows, edge 4) —
  this is expected and correct, not a bug: several rows inserted during this session's iterative
  debugging (`Dave`/`Erin`/`Frank`/`Grace`/`Henry`/`Ivy`) were deliberately left behind poisoned
  offsets and never propagated. The mismatch being detected accurately is itself proof `validate`'s
  row-count-compare logic works correctly against real data.

| Engine | Discover | Full-load | Validate | CDC | Cutover |
|---|---|---|---|---|---|
| Postgres | **live** | **live** | **live** | **live** | **live** |
| MySQL | **live** | **live** | **live** | **live** (incl. TIMESTAMP columns via the Debezium JDBC sink, see note) | **live** |
| MariaDB | **live** | **live** | **live** | **live** | **live** |
| MongoDB | **live** | **live** | **live** | **live** | **live** |
| SQL Server | **live** | via Debezium `initial` | advisory | **live** | **live** |
| Oracle | **live** | via Debezium `initial` | advisory | **live** (26ai) | **live** |

Fake path covers **all six** engines discover→cutover in CI (`tests/databridge_engines.rs`).

- **Lab `<ephemeral-ip>` (2026-07-28):** Strimzi + `zyvor-kafka` **Ready** (Kafka **4.3.0**; CR was
  bumped from unsupported 4.0.0), multi-engine Connect image imported, gateway
  `ATLAS_DATABRIDGE_CONNECT_IMAGE=localhost/databridge-connect:dev`. Fake MySQL plan walked
  assess→provision→full-load→**cdc_streaming**. Real MariaDB/Mongo CDC+cutover later verified on
  `212.8.248.187` (2026-09-01).
- **MySQL in-cluster pipeline through cutover — verified live (2026-10-04, `212.8.248.187`)**:
  single-node Rook Ceph (Squid, OSD on a loop device), CloudNativePG + Percona XtraDB + Strimzi
  (`zyvor-kafka`, Kafka 4.3.0), the real-Ceph gateway and the Debezium 3.7 Connect image. A MySQL
  8.4 source (binlog ROW, GTID) with a `TIMESTAMP DEFAULT CURRENT_TIMESTAMP` column and a
  composite-key table went discover → assess → provision (PXC edge, PVC on `zyvor-rbd-prod`) →
  full-load → `cdc/start` → validate (`passed`) → cutover (`draining` → `cutover_complete` in 4 s,
  connectors and the KafkaConnect cluster torn down). Inserts, updates and deletes on both tables
  landed on the edge identical to the source. The run surfaced two bugs, both fixed:
  `tls_mode: disable` sources failed discovery because sqlx 0.9 builds MySQL's RSA password
  exchange only behind `mysql-rsa` (#102), and the generated KafkaConnect had no heap or memory
  limit, so one worker grew to 5.4 GiB and drove the 32 GiB node into OOM (#103).
- **SQL Server 2022 and Oracle 26ai Free → Postgres through cutover, in-cluster — verified live
  (2026-10-04, same lab stack)**: each source went discover → assess → provision (CloudNativePG
  edge on `zyvor-rbd-prod`) → full-load (pass-through: the Debezium `initial` snapshot seeds the
  edge) → `cdc/start` → validate (advisory) → cutover (`cutover_complete`, connectors and the
  KafkaConnect cluster torn down). The snapshot rows plus an insert, an update and a delete landed
  on the edge identical to the source. Surfaced and fixed: the Ceph gateway image built the
  `oracle` feature without Oracle Instant Client, so discovery failed with DPI-1047 (#106), and a
  provision whose CR apply failed (CNPG webhook down) left the plan stuck in `provisioning` with no
  way to retry (#107). Note that the sink creates the edge table only when it processes the first
  snapshot records, which on a loaded host can be a minute after Debezium logs
  `Snapshot completed`.
- **Known limitation — writes during the full-load**: homogeneous relational CDC starts after the
  dump→restore, from the source's current WAL/binlog position (`no_data`), so a write made between
  the dump and the moment Debezium records that position reaches the edge only if the row changes
  again. That moment is later than `cdc/start` returning and later than the `KafkaConnector`s
  reporting `Ready`: on the lab run, an insert and an update made after both connectors were
  `Ready` were missed because the source task took its binlog position two seconds later. Wait
  for `Snapshot completed` / `Connected to binlog` in the Connect log (or the plan's CDC lag to
  report), or quiesce writes until then. `validate`'s row-count compare reports the drift.
- **CDC Connect image — multi-engine** (`deploy/databridge/connect/Dockerfile`): one Strimzi-based
  image bundles Debezium PostgreSQL + MySQL + **MariaDB** + MongoDB + Oracle + SQL Server source
  connectors (Debezium 3.7), the Debezium JDBC sink (every relational edge; ships its own drivers),
  the MongoDB Kafka sink, and the Aiven JDBC sink for sink connectors created by earlier releases
  (a re-apply moves them to the Debezium sink). `start_cdc` in real mode **refuses** without `ATLAS_DATABRIDGE_CONNECT_IMAGE` set. Lab Kafka
  CR: `deploy/databridge/10-kafka.yaml` (applied by `up.sh`). `up.sh` also installs the Percona
  Server for MongoDB operator for document-edge targets.

### Full-load secret assumptions
The real full-load Job runs in `zyvor-databridge`, so the **source Secret must exist in that
namespace** with `username`/`password` keys. The edge Secret is operator-generated: CloudNativePG's
`<cluster>-app` provides a ready-to-use `uri`; Percona XtraDB's `<cluster>-secrets` provides `root`;
PSMDB's `internal-<cluster>-users` provides `MONGODB_DATABASE_ADMIN_USER` /
`MONGODB_DATABASE_ADMIN_PASSWORD`.
