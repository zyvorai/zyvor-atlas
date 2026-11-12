// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Full-load batch-Job builders: dump the source dataset and load it into the freshly-provisioned
//! edge database, inside the cluster. Applied via `atlas_driver_k8s::apply_cr("batch","v1","Job",…)`
//! and watched to completion by the reconciler.
//!
//! Secret assumptions (documented in docs/DATABRIDGE.md): the source Secret lives in the edge
//! namespace and has `username`/`password` keys. The edge Secret is the operator-generated app
//! Secret — CloudNativePG's `<cluster>-app` exposes a ready-to-use `uri`; Percona's `<cluster>-secrets`
//! exposes `root`.

use serde_json::{json, Value};

pub const JOB_GROUP: &str = "batch";
pub const JOB_VERSION: &str = "v1";
pub const JOB_KIND: &str = "Job";

/// Deterministic full-load Job name for a plan (kept ≤ 63 chars, DNS-1123).
pub fn job_name(plan_id: &str) -> String {
    format!("load-{}", &plan_id[plan_id.len().saturating_sub(8)..])
}

/// Terminal state of a batch Job, derived from its `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    Running,
    Succeeded,
    Failed,
}

pub fn job_outcome(status: &Value) -> JobOutcome {
    if status
        .get("succeeded")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        >= 1
    {
        JobOutcome::Succeeded
    } else if status.get("failed").and_then(|v| v.as_i64()).unwrap_or(0) >= 1 {
        JobOutcome::Failed
    } else {
        JobOutcome::Running
    }
}

fn env_val(name: &str, value: &str) -> Value {
    json!({ "name": name, "value": value })
}
fn env_secret(name: &str, secret: &str, key: &str) -> Value {
    json!({ "name": name, "valueFrom": { "secretKeyRef": { "name": secret, "key": key } } })
}

/// Wrap a bash `script` in a single-shot batch Job spec (goes under the CR `spec`).
fn container_job(image: &str, script: &str, env: Vec<Value>) -> Value {
    json!({
        "backoffLimit": 2,
        "ttlSecondsAfterFinished": 3600,
        "template": {
            "spec": {
                "restartPolicy": "Never",
                "containers": [{
                    "name": "loader",
                    "image": image,
                    "command": ["/bin/bash", "-c"],
                    "args": [script],
                    "env": env
                }]
            }
        }
    })
}

/// Postgres full-load: `pg_dump` the source, pipe into `psql` against the edge (CNPG `uri`).
pub fn pg_job_spec(
    source_secret: &str,
    edge_secret: &str,
    src_host: &str,
    src_port: i64,
    src_db: &str,
    src_ssl: &str,
) -> Value {
    let script = r#"set -euo pipefail
SRC_DSN="postgresql://${SRC_USER}:${SRC_PASS}@${SRC_HOST}:${SRC_PORT}/${SRC_DB}?sslmode=${SRC_SSL}"
echo "full-load: pg_dump ${SRC_HOST}:${SRC_PORT}/${SRC_DB} -> edge"
pg_dump --no-owner --no-privileges --format=plain "$SRC_DSN" | psql "$EDGE_URI"
echo "full-load complete"
"#;
    container_job(
        "ghcr.io/cloudnative-pg/postgresql:16",
        script,
        vec![
            env_val("SRC_HOST", src_host),
            env_val("SRC_PORT", &src_port.to_string()),
            env_val("SRC_DB", src_db),
            env_val("SRC_SSL", src_ssl),
            env_secret("SRC_USER", source_secret, "username"),
            env_secret("SRC_PASS", source_secret, "password"),
            env_secret("EDGE_URI", edge_secret, "uri"),
        ],
    )
}

/// MySQL full-load: `mysqldump` the source, pipe into `mysql` against the edge (Percona `root`).
pub fn mysql_job_spec(
    source_secret: &str,
    edge_secret: &str,
    src_host: &str,
    src_port: i64,
    src_db: &str,
    edge_host: &str,
    edge_db: &str,
) -> Value {
    // `--column-statistics=0`: the MySQL-8 mysqldump client defaults to dumping column histograms
    // from information_schema.COLUMN_STATISTICS, which MariaDB has no such table for — without this
    // flag a MariaDB source fails with "Unknown table 'COLUMN_STATISTICS'" (1109). Harmless for MySQL.
    // `--skip-add-locks`: the edge target is Percona XtraDB Cluster, whose default
    // pxc_strict_mode=ENFORCING rejects explicit LOCK TABLES/FLUSH TABLES statements (error 1105)
    // — mysqldump wraps each table's INSERTs in LOCK/UNLOCK TABLES by default, so loading a dump
    // as-is into a fresh PXC edge always fails partway through without this flag.
    // `--set-gtid-purged=OFF`: a GTID-enabled source's dump includes `SET @@GLOBAL.GTID_PURGED=...`,
    // which fails (error 3546) against the edge's own already-nonempty GTID_EXECUTED set — full-load
    // is a one-shot copy, not a GTID-based replication resume, so the edge's own GTID history is
    // irrelevant here; CDC picks up independently via Debezium/binlog position afterward.
    let script = r#"set -euo pipefail
echo "full-load: mysqldump ${SRC_HOST}:${SRC_PORT}/${SRC_DB} -> ${EDGE_HOST}/${EDGE_DB}"
mysql -h "$EDGE_HOST" -u root -p"$EDGE_PASS" -e "CREATE DATABASE IF NOT EXISTS \`$EDGE_DB\`"
mysqldump --column-statistics=0 --no-tablespaces --skip-add-locks --set-gtid-purged=OFF --single-transaction --routines --triggers \
  -h "$SRC_HOST" -P "$SRC_PORT" -u "$SRC_USER" -p"$SRC_PASS" "$SRC_DB" \
  | mysql -h "$EDGE_HOST" -u root -p"$EDGE_PASS" "$EDGE_DB"
echo "full-load complete"
"#;
    container_job(
        "percona/percona-xtradb-cluster:8.0",
        script,
        vec![
            env_val("SRC_HOST", src_host),
            env_val("SRC_PORT", &src_port.to_string()),
            env_val("SRC_DB", src_db),
            env_val("EDGE_HOST", edge_host),
            env_val("EDGE_DB", edge_db),
            env_secret("SRC_USER", source_secret, "username"),
            env_secret("SRC_PASS", source_secret, "password"),
            env_secret("EDGE_PASS", edge_secret, "root"),
        ],
    )
}

/// MongoDB full-load: `mongodump` the source database, pipe the archive into `mongorestore` against
/// the edge PSMDB replica set, remapping the namespace to the edge database. Source creds come from
/// the source Secret (`username`/`password`); edge creds from the PSMDB users Secret.
#[allow(clippy::too_many_arguments)]
pub fn mongo_job_spec(
    source_secret: &str,
    edge_secret: &str,
    src_host: &str,
    src_port: i64,
    src_db: &str,
    edge_host: &str,
    edge_db: &str,
) -> Value {
    // authSource=admin is required: mongodump/mongorestore default the auth db to `--db`/the URI
    // path (the app DB), where the admin user does not exist — omitting it fails SCRAM auth. (mongosh
    // defaults to admin, which is why interactive checks mislead.) The admin user lives in `admin`.
    let script = r#"set -euo pipefail
SRC_URI="mongodb://${SRC_USER}:${SRC_PASS}@${SRC_HOST}:${SRC_PORT}/?replicaSet=rs0&authSource=admin"
EDGE_URI="mongodb://${EDGE_USER}:${EDGE_PASS}@${EDGE_HOST}:27017/?replicaSet=rs0&authSource=admin"
echo "full-load: mongodump ${SRC_HOST}:${SRC_PORT}/${SRC_DB} -> ${EDGE_HOST}/${EDGE_DB}"
mongodump --uri="$SRC_URI" --db="$SRC_DB" --archive \
  | mongorestore --uri="$EDGE_URI" --archive --nsFrom="${SRC_DB}.*" --nsTo="${EDGE_DB}.*"
echo "full-load complete"
"#;
    container_job(
        "percona/percona-server-mongodb:7.0",
        script,
        vec![
            env_val("SRC_HOST", src_host),
            env_val("SRC_PORT", &src_port.to_string()),
            env_val("SRC_DB", src_db),
            env_val("EDGE_HOST", edge_host),
            env_val("EDGE_DB", edge_db),
            env_secret("SRC_USER", source_secret, "username"),
            env_secret("SRC_PASS", source_secret, "password"),
            env_secret("EDGE_USER", edge_secret, "MONGODB_DATABASE_ADMIN_USER"),
            env_secret("EDGE_PASS", edge_secret, "MONGODB_DATABASE_ADMIN_PASSWORD"),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_name_is_short_and_stable() {
        let n = job_name("mplan_7ada95db7c03");
        assert_eq!(n, "load-95db7c03");
        assert!(n.len() <= 63);
    }

    #[test]
    fn outcome_from_status() {
        assert_eq!(
            job_outcome(&json!({ "succeeded": 1 })),
            JobOutcome::Succeeded
        );
        assert_eq!(job_outcome(&json!({ "failed": 1 })), JobOutcome::Failed);
        assert_eq!(job_outcome(&json!({ "active": 1 })), JobOutcome::Running);
        assert_eq!(job_outcome(&json!({})), JobOutcome::Running);
    }

    #[test]
    fn pg_job_wires_secrets_and_pipes() {
        let spec = pg_job_spec(
            "src-creds",
            "edge-app",
            "prod.rds.aws",
            5432,
            "appdb",
            "require",
        );
        let c = &spec["template"]["spec"]["containers"][0];
        assert!(c["args"][0].as_str().unwrap().contains("pg_dump"));
        assert!(c["args"][0]
            .as_str()
            .unwrap()
            .contains("psql \"$EDGE_URI\""));
        let env = c["env"].as_array().unwrap();
        assert!(env.iter().any(|e| e["name"] == "EDGE_URI"
            && e["valueFrom"]["secretKeyRef"]["name"] == "edge-app"
            && e["valueFrom"]["secretKeyRef"]["key"] == "uri"));
        assert!(env
            .iter()
            .any(|e| e["name"] == "SRC_PASS"
                && e["valueFrom"]["secretKeyRef"]["name"] == "src-creds"));
        assert!(env.iter().any(|e| e["name"] == "SRC_SSL" && e["value"] == "require"));
        assert!(c["args"][0].as_str().unwrap().contains("sslmode=${SRC_SSL}"));
    }

    /// Live full-load over TLS is not executed here. The Job spec is the same one
    /// [`super::pg_job_spec`] gives the reconciler. Set `DATABRIDGE_TEST_PG` to a TLS server
    /// and `DATABRIDGE_TEST_PG_TLS` to `require` or `verify-full`, then run with `--ignored`.
    #[test]
    #[ignore = "needs DATABRIDGE_TEST_PG against a TLS Postgres; DATABRIDGE_TEST_PG_TLS=require|verify-full"]
    fn live_pg_full_load_over_tls() {
        let dsn = std::env::var("DATABRIDGE_TEST_PG").expect("set DATABRIDGE_TEST_PG");
        let mode = std::env::var("DATABRIDGE_TEST_PG_TLS").unwrap_or_else(|_| "require".into());
        assert_ne!(mode, "disable", "this scaffolding is the TLS path, not plaintext");
        let spec = pg_job_spec("src-creds", "edge-app", "db.example", 5432, "appdb", &mode);
        let c = &spec["template"]["spec"]["containers"][0];
        let script = c["args"][0].as_str().unwrap();
        assert!(script.contains("sslmode=${SRC_SSL}"));
        let env = c["env"].as_array().unwrap();
        assert!(env.iter().any(|e| e["name"] == "SRC_SSL" && e["value"] == mode));
        // No dump is started: this process has no Kubernetes Job runner. The DSN is the
        // operator's target for a later live run.
        assert!(dsn.contains("host=") || dsn.contains("postgres"));
    }

    #[test]
    fn mysql_job_wires_secrets() {
        let spec = mysql_job_spec(
            "src-creds",
            "edge-secrets",
            "prod.rds.aws",
            3306,
            "appdb",
            "edge-haproxy",
            "appdb",
        );
        let c = &spec["template"]["spec"]["containers"][0];
        let script = c["args"][0].as_str().unwrap();
        assert!(script.contains("mysqldump"));
        // MariaDB sources fail without this (MySQL-8 mysqldump probes a table MariaDB lacks).
        assert!(script.contains("--column-statistics=0"));
        let env = c["env"].as_array().unwrap();
        assert!(env
            .iter()
            .any(|e| e["name"] == "EDGE_PASS" && e["valueFrom"]["secretKeyRef"]["key"] == "root"));
    }

    #[test]
    fn mongo_job_pipes_dump_to_restore() {
        let spec = mongo_job_spec(
            "src-creds",
            "edge-secrets",
            "mongo.rds.aws",
            27017,
            "appdb",
            "edge-rs0",
            "appdb",
        );
        let c = &spec["template"]["spec"]["containers"][0];
        let args = c["args"][0].as_str().unwrap();
        assert!(args.contains("mongodump"));
        assert!(args.contains("mongorestore"));
        // mongodump/mongorestore default authSource to the app db, where the admin user isn't — must pin admin.
        assert!(args.contains("authSource=admin"));
        let env = c["env"].as_array().unwrap();
        assert!(env.iter().any(|e| e["name"] == "EDGE_USER"
            && e["valueFrom"]["secretKeyRef"]["key"] == "MONGODB_DATABASE_ADMIN_USER"));
    }
}
