// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! Strimzi + Debezium CR builders for CDC: a `KafkaConnect` cluster (with the Debezium + JDBC-sink
//! plugins and a Kubernetes Secret config-provider) plus two `KafkaConnector`s — a Debezium source
//! (reads the cloud DB's WAL/binlog into Kafka) and a JDBC sink (applies the topics to the edge DB).
//!
//! NOTE: these specs are structurally correct but UNVERIFIED against a live Strimzi/Kafka + real DB
//! stack (see docs/DATABRIDGE.md). Secret values use Strimzi's config-provider syntax
//! `${secrets:<ns>/<secret>:<key>}` so credentials never appear in the CR.

use serde_json::{json, Value};

use crate::connector::SourceKind;

pub const GROUP: &str = "kafka.strimzi.io";
// Current Strimzi serves the Kafka/KafkaConnect/KafkaConnector kinds at v1 (v1beta2 was removed).
pub const VERSION: &str = "v1";
pub const CONNECT_KIND: &str = "KafkaConnect";
pub const CONNECTOR_KIND: &str = "KafkaConnector";

/// Names derived from a plan's short id, stable across reconciles.
pub fn connect_name(short: &str) -> String {
    format!("dbz-connect-{short}")
}
pub fn source_connector_name(short: &str) -> String {
    format!("dbz-src-{short}")
}
pub fn sink_connector_name(short: &str) -> String {
    format!("jdbc-sink-{short}")
}
pub fn topic_prefix(short: &str) -> String {
    format!("db{short}")
}

/// FNV-1a hash of a plan's short id, used to derive a MySQL/MariaDB `database.server.id` that's
/// unique per plan (short ids are all the same length, so hashing the length is not unique).
fn server_id_hash(short: &str) -> i64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for b in short.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    (hash % 1_000_000_000) as i64
}

/// A `KafkaConnect` cluster spec (Strimzi v1). `groupId` + the three storage topics are top-level
/// required fields; the Secret config-provider lets connectors reference `${secrets:…}`.
///
/// `image` must be a Kafka Connect image that BUNDLES the Debezium (postgres/mysql) + a JDBC-sink
/// plugin (e.g. built with Strimzi's `spec.build` + a registry, or a prebuilt image). The default
/// Strimzi Connect image has no connector plugins, so leaving `image` empty means connectors won't
/// instantiate — set `ATLAS_DATABRIDGE_CONNECT_IMAGE` at deploy time.
pub fn connect_spec(bootstrap_servers: &str, replicas: i64, image: Option<&str>) -> Value {
    let mut spec = json!({
        "replicas": replicas.max(1),
        "bootstrapServers": bootstrap_servers,
        "groupId": "zyvor-databridge-connect",
        "configStorageTopic": "zyvor-connect-configs",
        "offsetStorageTopic": "zyvor-connect-offsets",
        "statusStorageTopic": "zyvor-connect-status",
        // Strimzi's default (initialDelaySeconds=60, periodSeconds=10, failureThreshold=3 -> ~90s
        // to first kill) is too tight for a Connect image bundling Debezium + JDBC-sink plugins: JVM
        // boot + Kafka group-rebalance routinely takes 90-120s+ even on an idle host, and CI/lab hosts
        // under load push that further — Strimzi then repeatedly kills the pod mid-startup before it
        // ever reports healthy, so it can never actually come up.
        "livenessProbe": { "initialDelaySeconds": 180, "timeoutSeconds": 10, "periodSeconds": 15, "failureThreshold": 6 },
        "readinessProbe": { "initialDelaySeconds": 180, "timeoutSeconds": 10, "periodSeconds": 15, "failureThreshold": 6 },
        // Without a container limit the JVM sizes its heap from host RAM (a quarter of it); on a
        // 32 GiB edge node one Connect worker grew to 5.4 GiB RSS and drove the node into OOM.
        "jvmOptions": { "-Xms": "256m", "-Xmx": "1g" },
        "resources": { "requests": { "memory": "1Gi" }, "limits": { "memory": "2Gi" } },
        "config": {
            "config.providers": "secrets",
            "config.providers.secrets.class": "io.strimzi.kafka.KubernetesSecretConfigProvider",
            // Internal topics default to RF 3; a single-broker lab Kafka needs 1 or Connect can't
            // create them (/health 500 -> crashloop).
            "config.storage.replication.factor": 1,
            "offset.storage.replication.factor": 1,
            "status.storage.replication.factor": 1
        }
    });
    if let Some(img) = image.filter(|s| !s.is_empty()) {
        spec["image"] = json!(img);
    }
    spec
}

/// Debezium source connector config for the given engine. `secret_ns`/`secret` reference the source
/// credentials Secret (keys `username`/`password`).
///
/// Snapshot mode is derived from the engine: homogeneous sources (Postgres/MySQL/MariaDB) were
/// already seeded by the dump→restore full-load, so Debezium captures only the schema (`no_data`;
/// Debezium 3.7 removed `never`) and streams from there. Heterogeneous sources (Oracle/SQL Server →
/// Postgres) have no dump full-load — Debezium's `initial` snapshot seeds the edge (the JDBC sink
/// creates the tables), then it streams changes.
#[allow(clippy::too_many_arguments)]
pub fn debezium_source_spec(
    kind: SourceKind,
    short: &str,
    host: &str,
    port: i64,
    database: &str,
    secret_ns: &str,
    secret: &str,
) -> Value {
    // MongoDB is not a JDBC/SQL source — it uses a single connection string, not host/port/db/user
    // fields — so build it separately.
    if kind.is_document() {
        return mongo_debezium_source_spec(short, host, port, database, secret_ns, secret);
    }

    let user = format!("${{secrets:{secret_ns}/{secret}:username}}");
    let pass = format!("${{secrets:{secret_ns}/{secret}:password}}");
    let prefix = topic_prefix(short);
    let bootstrap = "zyvor-kafka-kafka-bootstrap:9092";
    let snapshot_mode = if kind.homogeneous() {
        "no_data"
    } else {
        "initial"
    };
    let mut config = json!({
        "connector.class": kind.debezium_class(),
        "tasks.max": 1,
        "database.hostname": host,
        "database.port": port,
        "database.user": user,
        "database.password": pass,
        "database.dbname": if kind == SourceKind::Oracle {
            crate::connector::oracle_containers(database).0
        } else {
            database
        },
        "topic.prefix": prefix,
        // Encode DECIMAL/NUMERIC as a double, not a VariableScaleDecimal STRUCT the JDBC sink can't
        // bind. Snapshot mode depends on the engine (see doc comment).
        "decimal.handling.mode": "double",
        "snapshot.mode": snapshot_mode,
        // `connect` mode emits DATE/TIME/DATETIME as Kafka Connect's logical Date/Time/Timestamp
        // types, which every JDBC sink binds as native temporal values. (MySQL `TIMESTAMP` is still
        // a Debezium ZonedTimestamp string — see jdbc_sink_spec.)
        "time.precision.mode": "connect",
    });
    match kind {
        SourceKind::Postgres => {
            config["plugin.name"] = json!("pgoutput");
            config["slot.name"] = json!(format!("dbz_{short}"));
            config["publication.autocreate.mode"] = json!("filtered");
        }
        SourceKind::Mysql | SourceKind::Mariadb => {
            // MySqlConnector / MariaDbConnector share the config keys. Must be unique per plan —
            // `short` is fixed-length, so hashing it (not `.len()`, which is constant) is required
            // or every plan collides on the same binlog replication client id.
            config["database.server.id"] = json!(184_000_000 + (server_id_hash(short) % 1_000_000));
            config["schema.history.internal.kafka.bootstrap.servers"] = json!(bootstrap);
            config["schema.history.internal.kafka.topic"] = json!(format!("dbz-history-{short}"));
        }
        SourceKind::Sqlserver => {
            // SQL Server captures per-database via `database.names`; it also keeps a schema history.
            config["database.names"] = json!(database);
            config["database.encrypt"] = json!(false);
            config["schema.history.internal.kafka.bootstrap.servers"] = json!(bootstrap);
            config["schema.history.internal.kafka.topic"] = json!(format!("dbz-history-{short}"));
        }
        SourceKind::Oracle => {
            // LogMiner mines the container database's redo; a multitenant source (`CDB/PDB`)
            // also names the PDB whose tables are captured.
            config["database.connection.adapter"] = json!("logminer");
            if let (_, Some(pdb)) = crate::connector::oracle_containers(database) {
                config["database.pdb.name"] = json!(pdb);
            }
            config["schema.history.internal.kafka.bootstrap.servers"] = json!(bootstrap);
            config["schema.history.internal.kafka.topic"] = json!(format!("dbz-history-{short}"));
        }
        // Handled by the is_document() early return above.
        SourceKind::Mongodb => unreachable!("mongodb built by mongo_debezium_source_spec"),
    }
    json!({ "class": config["connector.class"], "tasksMax": 1, "config": config })
}

/// Debezium **MongoDB** source connector config. Mongo takes a single `mongodb.connection.string`
/// (not host/port/user fields), reading the source's change streams / oplog. `snapshot.mode=no_data`
/// because `mongodump` seeds the edge in full-load. Requires
/// the source to be a replica set.
fn mongo_debezium_source_spec(
    short: &str,
    host: &str,
    port: i64,
    database: &str,
    secret_ns: &str,
    secret: &str,
) -> Value {
    let user = format!("${{secrets:{secret_ns}/{secret}:username}}");
    let pass = format!("${{secrets:{secret_ns}/{secret}:password}}");
    let prefix = topic_prefix(short);
    // authSource=admin, explicit for consistency with the full-load/validate Jobs (the admin user
    // lives in `admin`). The Java driver defaults path-less URIs to admin, but pinning it removes any
    // ambiguity across Mongo tooling — see the loader `authSource` fix.
    let conn = format!("mongodb://{user}:{pass}@{host}:{port}/?replicaSet=rs0&authSource=admin");
    let config = json!({
        "connector.class": "io.debezium.connector.mongodb.MongoDbConnector",
        "tasks.max": 1,
        "mongodb.connection.string": conn,
        "topic.prefix": prefix,
        "database.include.list": database,
        "capture.mode": "change_streams_update_full",
        // Skip the data snapshot; full-load already seeded the edge via mongodump.
        "snapshot.mode": "no_data",
    });
    json!({ "class": config["connector.class"], "tasksMax": 1, "config": config })
}

/// How a Debezium Mongo change-stream event should be treated.
///
/// `c`/`u`/`d`/`r` are document writes. `i` is an invalidate (collection drop or rename): it has no
/// `fullDocument`, and handing it to the oplog `MongoDbHandler` fails the task the same way an
/// update without `patch` does. The sink stays on `ChangeStreamHandler` and skips invalidates.
pub fn mongo_change_stream_disposition(op: &str) -> &'static str {
    match op {
        "c" | "u" | "d" | "r" => "apply",
        "i" => "skip_invalidate",
        _ => "skip_unknown",
    }
}

/// **MongoDB** sink connector config: the MongoDB Kafka Connect sink applying the Debezium Mongo CDC
/// topics to the edge Percona Server for MongoDB. `edge_host` is the edge replica-set service; the
/// user/pass come from the PSMDB users Secret via Strimzi's config-provider. Each change event is
/// written to `edge_db`, into the collection named by the event's own `source.collection`.
#[allow(clippy::too_many_arguments)]
pub fn mongo_sink_spec(
    short: &str,
    edge_host: &str,
    edge_db: &str,
    secret_ns: &str,
    edge_secret: &str,
    user_key: &str,
    pass_key: &str,
) -> Value {
    let prefix = topic_prefix(short);
    let user = format!("${{secrets:{secret_ns}/{edge_secret}:{user_key}}}");
    let pass = format!("${{secrets:{secret_ns}/{edge_secret}:{pass_key}}}");
    // authSource=admin, explicit — see mongo_debezium_source_spec / the loader authSource fix.
    let uri = format!("mongodb://{user}:{pass}@{edge_host}/?replicaSet=rs0&authSource=admin");
    let config = json!({
        "connector.class": "com.mongodb.kafka.connect.MongoSinkConnector",
        "tasks.max": 1,
        // Only `<prefix>.<db>.<collection>`. Routing topics to bare collection names would need a
        // bare-name alternative here, which also subscribes Connect's own config/offset/status
        // topics and kills the task on their non-JSON records.
        "topics.regex": format!("{prefix}[.][^.]+[.].*"),
        "connection.uri": uri,
        "database": edge_db,
        // Apply Debezium MongoDB change-stream events (the source's capture.mode) as insert/update/
        // delete. MongoDbHandler is for the removed oplog mode and fails updates ("missing `patch`").
        "change.data.capture.handler": "com.mongodb.kafka.connect.sink.cdc.debezium.mongodb.ChangeStreamHandler",
        "namespace.mapper": "com.mongodb.kafka.connect.sink.namespace.mapping.FieldPathNamespaceMapper",
        "namespace.mapper.value.collection.field": "source.collection",
        // Delete tombstones carry no value to map; they fall back to the default namespace, and
        // the CDC handler skips them (the preceding delete event already removed the document).
        "namespace.mapper.error": false,
        "consumer.override.auto.offset.reset": "earliest",
        "consumer.override.metadata.max.age.ms": "10000"
    });
    json!({ "class": "com.mongodb.kafka.connect.MongoSinkConnector", "tasksMax": 1, "config": config })
}

/// JDBC sink config applying the relational CDC topics to the edge DB (CNPG Postgres or Percona
/// XtraDB), using the Debezium JDBC sink. It consumes the Debezium envelope as-is, so Debezium's own
/// logical types bind — notably `io.debezium.time.ZonedTimestamp`, which Debezium always emits for
/// MySQL/MariaDB `TIMESTAMP` columns and which the Aiven sink cannot bind. Rows upsert on the record
/// key (every key column, so composite keys and Oracle's upper-case names need no `pk.fields`),
/// deletes are applied, and `schema.evolution=basic` creates the edge tables of a heterogeneous
/// migration (no dump full-load) and adds new columns to the pre-created ones of a homogeneous one.
pub fn jdbc_sink_spec(
    short: &str,
    jdbc_url: &str,
    secret_ns: &str,
    edge_secret: &str,
    edge_user: &str,
    edge_pass_key: &str,
) -> Value {
    let prefix = topic_prefix(short);
    let config = json!({
        "connector.class": DEBEZIUM_JDBC_SINK,
        "tasks.max": 1,
        "topics.regex": format!("{prefix}[.][^.]+[.].*"),
        "connection.url": jdbc_url,
        "connection.username": edge_user,
        "connection.password": format!("${{secrets:{secret_ns}/{edge_secret}:{edge_pass_key}}}"),
        "insert.mode": "upsert",
        "primary.key.mode": "record_key",
        "delete.enabled": true,
        "schema.evolution": "basic",
        "consumer.override.auto.offset.reset": "earliest",
        // topics.regex discovers new Debezium topics on a metadata refresh; shorten it from the 5min
        // default so a table's topic (created on first change) is picked up promptly (no restart).
        "consumer.override.metadata.max.age.ms": "10000",
        // The routed topic is the bare table name; the sink writes to the table of that name.
        "collection.name.format": "${topic}",
        "transforms": "route",
        "transforms.route.type": "org.apache.kafka.connect.transforms.RegexRouter",
        "transforms.route.regex": table_route_regex(&prefix),
        "transforms.route.replacement": "$1"
    });
    json!({ "class": DEBEZIUM_JDBC_SINK, "tasksMax": 1, "config": config })
}

/// Routes a relational CDC topic to its last segment, the bare table name. Topic shapes differ per
/// source: `<prefix>.<schema>.<table>` (Postgres, Oracle), `<prefix>.<db>.<table>` (MySQL/MariaDB)
/// and `<prefix>.<db>.<schema>.<table>` (SQL Server); keeping everything after the first segment
/// would make SQL Server rows target a `dbo.<table>` the edge does not have.
fn table_route_regex(prefix: &str) -> String {
    format!("{prefix}[.].*[.]([^.]+)")
}

const DEBEZIUM_JDBC_SINK: &str = "io.debezium.connector.jdbc.JdbcSinkConnector";

/// Whether a Strimzi `KafkaConnector` reports its connector `state == RUNNING`.
pub fn connector_running(status: &Value) -> bool {
    status
        .get("connectorStatus")
        .and_then(|c| c.get("connector"))
        .and_then(|c| c.get("state"))
        .and_then(|s| s.as_str())
        .map(|s| s.eq_ignore_ascii_case("RUNNING"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_stable() {
        assert_eq!(connect_name("abc123"), "dbz-connect-abc123");
        assert_eq!(source_connector_name("abc123"), "dbz-src-abc123");
        assert_eq!(topic_prefix("abc123"), "dbabc123");
    }

    #[test]
    fn connect_workers_have_a_bounded_heap_and_memory_limit() {
        let s = connect_spec("zyvor-kafka-kafka-bootstrap:9092", 1, None);
        assert_eq!(s["jvmOptions"]["-Xmx"], "1g");
        assert_eq!(s["resources"]["limits"]["memory"], "2Gi");
    }

    #[test]
    fn pg_source_uses_pgoutput_and_secret_provider() {
        let s = debezium_source_spec(
            SourceKind::Postgres,
            "abc123",
            "prod.rds.aws",
            5432,
            "appdb",
            "zyvor-databridge",
            "src-creds",
        );
        assert_eq!(
            s["config"]["connector.class"],
            "io.debezium.connector.postgresql.PostgresConnector"
        );
        assert_eq!(s["config"]["plugin.name"], "pgoutput");
        assert_eq!(s["config"]["topic.prefix"], "dbabc123");
        assert_eq!(s["config"]["snapshot.mode"], "no_data"); // homogeneous — full-load seeded it
        assert_eq!(
            s["config"]["database.password"],
            "${secrets:zyvor-databridge/src-creds:password}"
        );
    }

    #[test]
    fn mysql_source_sets_server_id_and_history() {
        let s = debezium_source_spec(
            SourceKind::Mysql,
            "abc123",
            "prod.rds.aws",
            3306,
            "appdb",
            "zyvor-databridge",
            "src-creds",
        );
        assert_eq!(
            s["config"]["connector.class"],
            "io.debezium.connector.mysql.MySqlConnector"
        );
        assert!(s["config"]["database.server.id"].as_i64().unwrap() > 0);
        assert!(s["config"]["schema.history.internal.kafka.topic"]
            .as_str()
            .unwrap()
            .contains("abc123"));
    }

    #[test]
    fn mariadb_uses_its_own_connector_class() {
        let s = debezium_source_spec(
            SourceKind::Mariadb,
            "abc123",
            "h",
            3306,
            "appdb",
            "ns",
            "creds",
        );
        assert_eq!(
            s["config"]["connector.class"],
            "io.debezium.connector.mariadb.MariaDbConnector"
        );
        assert!(s["config"]["database.server.id"].as_i64().unwrap() > 0);
    }

    #[test]
    fn heterogeneous_sources_snapshot_initial() {
        let ora = debezium_source_spec(
            SourceKind::Oracle,
            "abc123",
            "h",
            1521,
            "ORCLCDB/ORCLPDB",
            "ns",
            "creds",
        );
        assert_eq!(
            ora["config"]["connector.class"],
            "io.debezium.connector.oracle.OracleConnector"
        );
        assert_eq!(ora["config"]["snapshot.mode"], "initial");
        assert_eq!(ora["config"]["database.dbname"], "ORCLCDB");
        assert_eq!(ora["config"]["database.pdb.name"], "ORCLPDB");
        let non_cdb =
            debezium_source_spec(SourceKind::Oracle, "abc123", "h", 1521, "ORCL", "ns", "c");
        assert_eq!(non_cdb["config"]["database.dbname"], "ORCL");
        assert!(non_cdb["config"].get("database.pdb.name").is_none());
        let mss = debezium_source_spec(
            SourceKind::Sqlserver,
            "abc123",
            "h",
            1433,
            "appdb",
            "ns",
            "creds",
        );
        assert_eq!(
            mss["config"]["connector.class"],
            "io.debezium.connector.sqlserver.SqlServerConnector"
        );
        assert_eq!(mss["config"]["snapshot.mode"], "initial");
        assert_eq!(mss["config"]["database.names"], "appdb");
    }

    #[test]
    fn sink_takes_the_debezium_envelope() {
        let s = jdbc_sink_spec(
            "abc123",
            "jdbc:mysql://edge-haproxy:3306/shop",
            "ns",
            "edge-secrets",
            "root",
            "root",
        );
        let c = &s["config"];
        assert_eq!(s["class"], "io.debezium.connector.jdbc.JdbcSinkConnector");
        assert_eq!(c["connection.username"], "root");
        assert_eq!(c["connection.password"], "${secrets:ns/edge-secrets:root}");
        assert_eq!(c["insert.mode"], "upsert");
        assert_eq!(c["primary.key.mode"], "record_key");
        assert_eq!(c["delete.enabled"], true);
        assert_eq!(c["schema.evolution"], "basic");
        assert_eq!(c["topics.regex"], "dbabc123[.][^.]+[.].*");
        assert_eq!(
            c["transforms"], "route",
            "no unwrap: the sink reads the envelope"
        );
        assert!(c.get("pk.fields").is_none());
    }

    #[test]
    fn sink_routes_every_topic_shape_to_its_table() {
        let s = jdbc_sink_spec(
            "abc123",
            "jdbc:postgresql://e/appdb",
            "ns",
            "s",
            "app",
            "pw",
        );
        let route = regex_lite(s["config"]["transforms.route.regex"].as_str().unwrap());
        for (topic, table) in [
            ("dbabc123.shop.dbo.customers", "customers"),
            ("dbabc123.public.orders", "orders"),
            ("dbabc123.APP.CUSTOMERS", "CUSTOMERS"),
        ] {
            assert_eq!(route(topic).as_deref(), Some(table), "{topic}");
        }
        assert_eq!(route("dbother.public.orders"), None);
    }

    /// Just enough of RegexRouter for `<prefix>[.].*[.]([^.]+)`: the last dot-separated segment
    /// of a topic that starts with `<prefix>.`.
    fn regex_lite(re: &str) -> impl Fn(&str) -> Option<String> + '_ {
        let prefix = re
            .strip_suffix("[.].*[.]([^.]+)")
            .expect("route regex shape");
        move |topic: &str| {
            let rest = topic.strip_prefix(prefix)?.strip_prefix('.')?;
            let (_, table) = rest.rsplit_once('.')?;
            Some(table.to_string())
        }
    }

    #[test]
    fn mongo_source_uses_connection_string_not_jdbc() {
        let s = debezium_source_spec(
            SourceKind::Mongodb,
            "abc123",
            "mongo.rds.aws",
            27017,
            "appdb",
            "ns",
            "src-creds",
        );
        assert_eq!(
            s["config"]["connector.class"],
            "io.debezium.connector.mongodb.MongoDbConnector"
        );
        let conn = s["config"]["mongodb.connection.string"].as_str().unwrap();
        assert!(conn.contains("mongo.rds.aws:27017"));
        assert!(conn.contains("replicaSet=rs0"));
        assert!(conn.contains("authSource=admin"));
        assert!(s["config"].get("database.hostname").is_none());
        assert_eq!(s["config"]["snapshot.mode"], "no_data");
    }

    #[test]
    fn mongo_invalidate_is_not_applied_as_a_document_write() {
        assert_eq!(mongo_change_stream_disposition("c"), "apply");
        assert_eq!(mongo_change_stream_disposition("u"), "apply");
        assert_eq!(mongo_change_stream_disposition("d"), "apply");
        assert_eq!(mongo_change_stream_disposition("r"), "apply");
        assert_eq!(mongo_change_stream_disposition("i"), "skip_invalidate");
        assert_eq!(mongo_change_stream_disposition(""), "skip_unknown");
        assert_ne!(mongo_change_stream_disposition("i"), "apply");
    }

    #[test]
    fn mongo_sink_routes_topics_to_collections() {
        let s = mongo_sink_spec(
            "abc123",
            "edge-rs0.zyvor-databridge.svc:27017",
            "appdb",
            "ns",
            "edge-secrets",
            "MONGODB_DATABASE_ADMIN_USER",
            "MONGODB_DATABASE_ADMIN_PASSWORD",
        );
        assert_eq!(
            s["config"]["connector.class"],
            "com.mongodb.kafka.connect.MongoSinkConnector"
        );
        assert_eq!(s["config"]["database"], "appdb");
        assert!(s["config"]["connection.uri"]
            .as_str()
            .unwrap()
            .contains("edge-rs0"));
        assert!(s["config"]["connection.uri"]
            .as_str()
            .unwrap()
            .contains("authSource=admin"));
        assert_eq!(
            s["config"]["change.data.capture.handler"],
            "com.mongodb.kafka.connect.sink.cdc.debezium.mongodb.ChangeStreamHandler"
        );
        assert_eq!(s["config"]["topics.regex"], "dbabc123[.][^.]+[.].*");
        assert_eq!(
            s["config"]["namespace.mapper.value.collection.field"],
            "source.collection"
        );
        assert!(s["config"].get("transforms").is_none());
    }

    #[test]
    fn running_status_parsed() {
        assert!(connector_running(
            &json!({ "connectorStatus": { "connector": { "state": "RUNNING" } } })
        ));
        assert!(!connector_running(
            &json!({ "connectorStatus": { "connector": { "state": "FAILED" } } })
        ));
        assert!(!connector_running(&json!({})));
    }
}
