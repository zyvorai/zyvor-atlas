// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//! Validation batch-Job builders: compare row counts of every table between the source and the edge
//! DB, inside the cluster. Applied via `apply_cr("batch","v1","Job",…)` and watched by the reconciler
//! (Job exit 0 = passed, non-zero = mismatch/failed). Per-table detail from logs is a follow-up.
//!
//! Same Secret assumptions as the loader (see docs/DATABRIDGE.md).

use serde_json::{json, Value};

/// Deterministic validation Job name for a validation-run id.
pub fn job_name(validation_id: &str) -> String {
    format!(
        "validate-{}",
        &validation_id[validation_id.len().saturating_sub(8)..]
    )
}

fn env_val(name: &str, value: &str) -> Value {
    json!({ "name": name, "value": value })
}
fn env_secret(name: &str, secret: &str, key: &str) -> Value {
    json!({ "name": name, "valueFrom": { "secretKeyRef": { "name": secret, "key": key } } })
}

fn container_job(image: &str, script: &str, env: Vec<Value>) -> Value {
    json!({
        "backoffLimit": 1,
        "ttlSecondsAfterFinished": 3600,
        "template": { "spec": {
            "restartPolicy": "Never",
            "containers": [{
                "name": "validate", "image": image,
                "command": ["/bin/bash", "-c"], "args": [script], "env": env
            }]
        }}
    })
}

/// Postgres: compare `count(*)` per public table between source and edge; exit non-zero on mismatch.
pub fn pg_validate_job_spec(
    source_secret: &str,
    edge_secret: &str,
    src_host: &str,
    src_port: i64,
    src_db: &str,
    src_ssl: &str,
) -> Value {
    let script = r#"set -euo pipefail
SRC_DSN="postgresql://${SRC_USER}:${SRC_PASS}@${SRC_HOST}:${SRC_PORT}/${SRC_DB}?sslmode=${SRC_SSL}"
tables=$(psql "$SRC_DSN" -Atc "SELECT schemaname||'.'||tablename FROM pg_tables WHERE schemaname='public'")
fail=0
for t in $tables; do
  sc=$(psql "$SRC_DSN" -Atc "SELECT count(*) FROM $t")
  ec=$(psql "$EDGE_URI" -Atc "SELECT count(*) FROM $t")
  echo "$t source=$sc edge=$ec"
  if [ "$sc" != "$ec" ]; then echo "MISMATCH $t"; fail=1; fi
done
exit $fail
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

/// MySQL: compare `count(*)` per table between source and edge; exit non-zero on mismatch.
pub fn mysql_validate_job_spec(
    source_secret: &str,
    edge_secret: &str,
    src_host: &str,
    src_port: i64,
    src_db: &str,
    edge_host: &str,
    edge_db: &str,
) -> Value {
    let script = r#"set -euo pipefail
SRC="mysql -h $SRC_HOST -P $SRC_PORT -u $SRC_USER -p$SRC_PASS -N -B $SRC_DB"
EDGE="mysql -h $EDGE_HOST -u root -p$EDGE_PASS -N -B $EDGE_DB"
tables=$($SRC -e "SELECT table_name FROM information_schema.tables WHERE table_schema='$SRC_DB'")
fail=0
for t in $tables; do
  sc=$($SRC -e "SELECT count(*) FROM \`$t\`")
  ec=$($EDGE -e "SELECT count(*) FROM \`$t\`")
  echo "$t source=$sc edge=$ec"
  if [ "$sc" != "$ec" ]; then echo "MISMATCH $t"; fail=1; fi
done
exit $fail
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

/// MongoDB: compare `countDocuments` per collection between source and edge; exit non-zero on
/// mismatch. Uses `mongosh` (source creds `username`/`password`, edge creds from the PSMDB Secret).
#[allow(clippy::too_many_arguments)]
pub fn mongo_validate_job_spec(
    source_secret: &str,
    edge_secret: &str,
    src_host: &str,
    src_port: i64,
    src_db: &str,
    edge_host: &str,
    edge_db: &str,
) -> Value {
    // authSource=admin: the URI names a db (SRC_DB/EDGE_DB), so without it the auth db defaults to
    // the app db — where the admin user does not exist — and SCRAM auth fails. The admin user is in `admin`.
    let script = r#"set -euo pipefail
SRC="mongodb://${SRC_USER}:${SRC_PASS}@${SRC_HOST}:${SRC_PORT}/${SRC_DB}?replicaSet=rs0&authSource=admin"
EDGE="mongodb://${EDGE_USER}:${EDGE_PASS}@${EDGE_HOST}:27017/${EDGE_DB}?replicaSet=rs0&authSource=admin"
cols=$(mongosh "$SRC" --quiet --eval 'db.getCollectionNames().join("\n")')
fail=0
for c in $cols; do
  sc=$(mongosh "$SRC" --quiet --eval "db.getCollection('$c').countDocuments({})")
  ec=$(mongosh "$EDGE" --quiet --eval "db.getCollection('$c').countDocuments({})")
  echo "$c source=$sc edge=$ec"
  if [ "$sc" != "$ec" ]; then echo "MISMATCH $c"; fail=1; fi
done
exit $fail
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
    fn job_name_short() {
        assert_eq!(job_name("val_1234567890ab"), "validate-567890ab");
    }

    #[test]
    fn pg_validate_compares_counts() {
        let spec = pg_validate_job_spec("src", "edge-app", "h", 5432, "appdb", "require");
        let args = spec["template"]["spec"]["containers"][0]["args"][0]
            .as_str()
            .unwrap();
        assert!(args.contains("SELECT count(*)"));
        assert!(args.contains("MISMATCH"));
        let env = spec["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap();
        assert!(env.iter().any(|e| e["name"] == "EDGE_URI"));
    }

    #[test]
    fn mysql_validate_uses_information_schema() {
        let spec = mysql_validate_job_spec(
            "src",
            "edge-secrets",
            "h",
            3306,
            "appdb",
            "edge-haproxy",
            "appdb",
        );
        let args = spec["template"]["spec"]["containers"][0]["args"][0]
            .as_str()
            .unwrap();
        assert!(args.contains("information_schema.tables"));
    }

    #[test]
    fn mongo_validate_counts_documents() {
        let spec = mongo_validate_job_spec(
            "src",
            "edge-secrets",
            "h",
            27017,
            "appdb",
            "edge-rs0",
            "appdb",
        );
        let args = spec["template"]["spec"]["containers"][0]["args"][0]
            .as_str()
            .unwrap();
        assert!(args.contains("countDocuments"));
        assert!(args.contains("MISMATCH"));
        assert!(args.contains("authSource=admin"));
    }
}
