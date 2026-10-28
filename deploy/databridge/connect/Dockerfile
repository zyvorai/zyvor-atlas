# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# Kafka Connect image for DataBridge CDC: Strimzi Kafka base + Debezium source connectors
# (PostgreSQL, MySQL/MariaDB, MongoDB, Oracle, SQL Server) + the Debezium JDBC sink (every
# relational edge: it understands Debezium's own temporal types such as ZonedTimestamp for MySQL
# TIMESTAMP columns, and ships the Postgres/MySQL/MariaDB/SQL Server/Oracle drivers) + the
# MongoDB Kafka sink. The Aiven JDBC sink stays for sink connectors created by DataBridge
# releases before the Debezium sink; a re-apply moves them over. One image serves every
# DataBridge engine. Point the gateway at it with ATLAS_DATABRIDGE_CONNECT_IMAGE so
# start_cdc's KafkaConnect can instantiate the connectors.
#
# Build + import into k3s (on a cluster node):
#   podman build -t databridge-connect:dev -f deploy/databridge/connect/Dockerfile deploy/databridge/connect
#   podman save databridge-connect:dev -o /tmp/c.tar && sudo k3s ctr images import /tmp/c.tar
# Base must match the Strimzi operator's Kafka version (4.x: log4j2 layout).
# Debezium 3.0.8.Final is NOT compatible with a Kafka 4.x client: its KafkaSchemaHistory recovery
# path calls the deprecated KafkaConsumer.poll(long) overload, which Kafka 4.0 removed outright —
# every MySQL/MariaDB/Oracle/SQL Server source connector (any engine using schema history) fails at
# task startup with `NoSuchMethodError: KafkaConsumer.poll(long)`. 3.3.0.Final is the first line on
# this base image's Kafka 4.3 that's actually built/tested against Kafka Connect 4.x.
# Oracle 26ai needs 3.7+: older Oracle connectors look the version up with
# `BANNER_FULL LIKE 'Oracle Database%'`, which the "Oracle AI Database 26ai" banner never matches,
# so validation fails with "Failed to resolve Oracle database version".
FROM quay.io/strimzi/kafka:latest-kafka-4.4.0@sha256:a5656a4562116043d81e6ed5233503ae2c38d55b9d81dfac1f4f31f31a27abed
USER root

ARG DEBEZIUM_VERSION=3.7.0.Final
ARG AIVEN_JDBC_VERSION=6.10.0
ARG PG_JDBC_VERSION=42.7.3
ARG MYSQL_JDBC_VERSION=8.4.0
ARG MSSQL_JDBC_VERSION=12.8.1.jre11
ARG ORACLE_JDBC_VERSION=23.5.0.24.07
ARG MONGO_SINK_VERSION=1.15.0

RUN mkdir -p /opt/kafka/plugins/debezium-postgres /opt/kafka/plugins/debezium-mysql \
             /opt/kafka/plugins/debezium-mariadb /opt/kafka/plugins/debezium-mongodb \
             /opt/kafka/plugins/debezium-oracle /opt/kafka/plugins/debezium-sqlserver \
             /opt/kafka/plugins/jdbc-sink /opt/kafka/plugins/debezium-jdbc-sink \
             /opt/kafka/plugins/mongo-sink

# Debezium source connectors: PostgreSQL, MySQL, MariaDB, MongoDB, Oracle, SQL Server.
RUN curl -fsSL "https://repo1.maven.org/maven2/io/debezium/debezium-connector-postgres/${DEBEZIUM_VERSION}/debezium-connector-postgres-${DEBEZIUM_VERSION}-plugin.tar.gz" \
    | tar -xz -C /opt/kafka/plugins/debezium-postgres --strip-components=1
RUN curl -fsSL "https://repo1.maven.org/maven2/io/debezium/debezium-connector-mysql/${DEBEZIUM_VERSION}/debezium-connector-mysql-${DEBEZIUM_VERSION}-plugin.tar.gz" \
    | tar -xz -C /opt/kafka/plugins/debezium-mysql --strip-components=1
RUN curl -fsSL "https://repo1.maven.org/maven2/io/debezium/debezium-connector-mariadb/${DEBEZIUM_VERSION}/debezium-connector-mariadb-${DEBEZIUM_VERSION}-plugin.tar.gz" \
    | tar -xz -C /opt/kafka/plugins/debezium-mariadb --strip-components=1
RUN curl -fsSL "https://repo1.maven.org/maven2/io/debezium/debezium-connector-mongodb/${DEBEZIUM_VERSION}/debezium-connector-mongodb-${DEBEZIUM_VERSION}-plugin.tar.gz" \
    | tar -xz -C /opt/kafka/plugins/debezium-mongodb --strip-components=1
RUN curl -fsSL "https://repo1.maven.org/maven2/io/debezium/debezium-connector-oracle/${DEBEZIUM_VERSION}/debezium-connector-oracle-${DEBEZIUM_VERSION}-plugin.tar.gz" \
    | tar -xz -C /opt/kafka/plugins/debezium-oracle --strip-components=1
RUN curl -fsSL "https://repo1.maven.org/maven2/io/debezium/debezium-connector-sqlserver/${DEBEZIUM_VERSION}/debezium-connector-sqlserver-${DEBEZIUM_VERSION}-plugin.tar.gz" \
    | tar -xz -C /opt/kafka/plugins/debezium-sqlserver --strip-components=1

# Aiven JDBC sink connector (legacy sink connectors only, see above) + its JDBC drivers.
RUN curl -fsSL -o /tmp/jdbc.tar "https://github.com/Aiven-Open/jdbc-connector-for-apache-kafka/releases/download/v${AIVEN_JDBC_VERSION}/jdbc-connector-for-apache-kafka-${AIVEN_JDBC_VERSION}.tar" \
    && tar -xf /tmp/jdbc.tar -C /opt/kafka/plugins/jdbc-sink --strip-components=1 \
    && rm -f /tmp/jdbc.tar \
    && curl -fsSL -o /opt/kafka/plugins/jdbc-sink/postgresql.jar "https://jdbc.postgresql.org/download/postgresql-${PG_JDBC_VERSION}.jar" \
    && curl -fsSL -o /opt/kafka/plugins/jdbc-sink/mysql-connector-j.jar "https://repo1.maven.org/maven2/com/mysql/mysql-connector-j/${MYSQL_JDBC_VERSION}/mysql-connector-j-${MYSQL_JDBC_VERSION}.jar" \
    && curl -fsSL -o /opt/kafka/plugins/jdbc-sink/mssql-jdbc.jar "https://repo1.maven.org/maven2/com/microsoft/sqlserver/mssql-jdbc/${MSSQL_JDBC_VERSION}/mssql-jdbc-${MSSQL_JDBC_VERSION}.jar" \
    && curl -fsSL -o /opt/kafka/plugins/jdbc-sink/ojdbc11.jar "https://repo1.maven.org/maven2/com/oracle/database/jdbc/ojdbc11/${ORACLE_JDBC_VERSION}/ojdbc11-${ORACLE_JDBC_VERSION}.jar"

# Debezium JDBC sink (io.debezium.connector.jdbc.JdbcSinkConnector); the archive bundles its drivers.
RUN curl -fsSL "https://repo1.maven.org/maven2/io/debezium/debezium-connector-jdbc/${DEBEZIUM_VERSION}/debezium-connector-jdbc-${DEBEZIUM_VERSION}-plugin.tar.gz" \
    | tar -xz -C /opt/kafka/plugins/debezium-jdbc-sink --strip-components=1 \
    && ls /opt/kafka/plugins/debezium-jdbc-sink | grep -q '^postgresql-' \
    && ls /opt/kafka/plugins/debezium-jdbc-sink | grep -q '^mysql-connector-j'

# MongoDB Kafka sink connector (com.mongodb.kafka.connect.MongoSinkConnector), fat "-all" jar.
RUN curl -fsSL -o /opt/kafka/plugins/mongo-sink/mongo-kafka-connect-all.jar \
    "https://repo1.maven.org/maven2/org/mongodb/kafka/mongo-kafka-connect/${MONGO_SINK_VERSION}/mongo-kafka-connect-${MONGO_SINK_VERSION}-all.jar"

USER 1001
