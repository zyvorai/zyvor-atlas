<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# postgres-lab

Throwaway Postgres for verifying atlas-inventory's query layer (`connect()`/`migrate()`, see
`docs/HA.md`) against real infrastructure, not just a compile-check against `sqlx::Any`.

```
./up.sh
```

Stands up, in the lab k3s cluster's `zyvor-system` namespace:
- A generated-password Secret (`postgres-lab-auth`), never committed or printed by the script
- `postgres:16-alpine`, single replica, ephemeral `local-path` PVC
- A NodePort Service on `30432`

Then run the live verification test:

```
DATABASE_URL="$(kubectl -n zyvor-system get secret postgres-lab-auth -o jsonpath='{.data.database-url}' | base64 -d | sed 's#postgres-lab.zyvor-system.svc:5432#<NODE_IP>:30432#')"
cargo test -p atlas-inventory --test postgres_live -- --ignored --nocapture
```

## What this proves, and what it doesn't

Proves: `connect()` opens a real Postgres connection, `migrate()` runs `migrations-postgres/`
clean against it, and the query layer actually works there too — `postgres_live.rs` exercises the
job-claim/reclaim/retry state machine (`jobs.rs`) and case-insensitive username lookup
(`users.rs`), the two places this migration found real SQLite-vs-Postgres behavioral differences
(a SQLite-only scalar `MAX(a, b)` and `COLLATE NOCASE`), not just connect+migrate.

Does **not** prove: full multi-replica HA readiness. A real HA deployment also needs
replication/failover (e.g. Patroni, CloudNativePG) and the remaining items in `docs/HA.md`'s
"Remaining for a full multi-replica cutover" list (dual-backend CI, DB-backed rate limiting, Helm
`database.kind`) — this single-replica lab target is a query-layer verification smoke test, not an
HA reference topology.

## Adopting this for real

A bank pilot would point `DATABASE_URL` at their own managed/HA Postgres (RDS, CloudNativePG,
Patroni-managed, etc.) instead of this lab container — the `connect()`/`migrate()` code path
doesn't change, only the connection string and the operational guarantees behind it.

## Teardown

```
kubectl -n zyvor-system delete deploy/postgres-lab svc/postgres-lab pvc/postgres-lab-data secret/postgres-lab-auth
```
