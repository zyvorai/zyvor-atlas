#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Verify the DataBridge REAL source connectors against ephemeral database containers. For each engine
# this spins a throwaway instance, seeds a tiny `customers`/`orders` schema, exports the matching
# DATABRIDGE_TEST_* var, and runs that engine's env-gated `#[tokio::test]` discovery test (which is a
# no-op skip when the var is unset). Containers are always torn down on exit.
#
# Engines: postgres, mysql, mariadb, mongodb (default feature / feature `mongodb`), and sqlserver
# (feature `sqlserver`). Oracle is compile-only (needs the OCI client) and is not covered here.
#
# Usage:
#   scripts/test-connectors.sh                 # all engines
#   scripts/test-connectors.sh pg mysql        # a subset
#   RUNTIME=docker scripts/test-connectors.sh  # force docker instead of podman
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$HERE"

RUNTIME="${RUNTIME:-$(command -v podman >/dev/null && echo podman || echo docker)}"
PREFIX="atlas-conn"
ENGINES=("$@"); [[ ${#ENGINES[@]} -eq 0 ]] && ENGINES=(pg mysql mariadb mongo mssql)

# Fixed host ports (unlikely to clash with a local dev DB on the defaults).
PG_PORT=55432; MYSQL_PORT=53306; MARIA_PORT=53307; MONGO_PORT=57017; MSSQL_PORT=51433
MSSQL_SA_PASS='Atlas_Str0ng!Pass'

log()  { printf '\033[1;36m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m!! %s\033[0m\n' "$*"; }
cleanup() { for e in pg mysql mariadb mongo mssql; do $RUNTIME rm -f "${PREFIX}-${e}" >/dev/null 2>&1 || true; done; }
trap cleanup EXIT

# Retry a command up to N times, sleeping 2s between attempts.
wait_for() { local n="$1"; shift; for _ in $(seq 1 "$n"); do "$@" >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }

FAILED=()

want() { for e in "${ENGINES[@]}"; do [[ "$e" == "$1" ]] && return 0; done; return 1; }

run_test() { # <feature-args...> -- <test-name> ; env var already exported by caller
  local args=(); while [[ "$1" != "--" ]]; do args+=("$1"); shift; done; shift
  cargo test -p atlas-databridge "${args[@]}" "$1" -- --exact --nocapture
}

# ---------- Postgres ----------
if want pg; then
  log "postgres:16"
  $RUNTIME run -d --name "${PREFIX}-pg" -e POSTGRES_PASSWORD=atlas -p ${PG_PORT}:5432 \
    postgres:16 -c wal_level=logical >/dev/null
  wait_for 30 $RUNTIME exec "${PREFIX}-pg" pg_isready -U postgres || { warn "pg not ready"; FAILED+=(pg); }
  $RUNTIME exec "${PREFIX}-pg" psql -U postgres -v ON_ERROR_STOP=1 -c \
    "CREATE TABLE customers(id serial PRIMARY KEY, name text);
     CREATE TABLE orders(id serial PRIMARY KEY, customer_id int);
     INSERT INTO customers(name) VALUES ('a'),('b');" >/dev/null
  export DATABRIDGE_TEST_PG="host=127.0.0.1 port=${PG_PORT} dbname=postgres user=postgres password=atlas sslmode=disable"
  run_test -- discovers_real_postgres || FAILED+=(pg)
  unset DATABRIDGE_TEST_PG
fi

# ---------- MySQL ----------
if want mysql; then
  log "mysql:8 (binlog ROW)"
  $RUNTIME run -d --name "${PREFIX}-mysql" -e MYSQL_ROOT_PASSWORD=atlas -e MYSQL_ROOT_HOST=% \
    -p ${MYSQL_PORT}:3306 mysql:8 --log-bin=mysql-bin --binlog-format=ROW --server-id=1 >/dev/null
  wait_for 45 $RUNTIME exec "${PREFIX}-mysql" mysqladmin ping -uroot -patlas --silent || { warn "mysql not ready"; FAILED+=(mysql); }
  $RUNTIME exec "${PREFIX}-mysql" mysql -uroot -patlas -e \
    "CREATE DATABASE appdb;
     CREATE TABLE appdb.customers(id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100));
     CREATE TABLE appdb.orders(id INT AUTO_INCREMENT PRIMARY KEY, customer_id INT);
     INSERT INTO appdb.customers(name) VALUES ('a'),('b');" >/dev/null
  export DATABRIDGE_TEST_MYSQL="127.0.0.1,${MYSQL_PORT},appdb,root,atlas"
  run_test -- discovers_real_mysql || FAILED+=(mysql)
  unset DATABRIDGE_TEST_MYSQL
fi

# ---------- MariaDB ----------
if want mariadb; then
  log "mariadb:11 (binlog ROW)"
  $RUNTIME run -d --name "${PREFIX}-mariadb" -e MARIADB_ROOT_PASSWORD=atlas \
    -p ${MARIA_PORT}:3306 mariadb:11 --log-bin --binlog-format=ROW --server-id=1 >/dev/null
  wait_for 45 $RUNTIME exec "${PREFIX}-mariadb" mariadb-admin ping -uroot -patlas --silent || { warn "mariadb not ready"; FAILED+=(mariadb); }
  $RUNTIME exec "${PREFIX}-mariadb" mariadb -uroot -patlas -e \
    "CREATE DATABASE appdb;
     CREATE TABLE appdb.customers(id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100));
     INSERT INTO appdb.customers(name) VALUES ('a'),('b');" >/dev/null
  export DATABRIDGE_TEST_MARIADB="127.0.0.1,${MARIA_PORT},appdb,root,atlas"
  run_test -- discovers_real_mariadb || FAILED+=(mariadb)
  unset DATABRIDGE_TEST_MARIADB
fi

# ---------- MongoDB (replica set, no auth) ----------
if want mongo; then
  log "mongo:7 (replSet rs0)"
  $RUNTIME run -d --name "${PREFIX}-mongo" -p ${MONGO_PORT}:27017 mongo:7 --replSet rs0 >/dev/null
  wait_for 30 $RUNTIME exec "${PREFIX}-mongo" mongosh --quiet --eval 'db.runCommand({ping:1})' || { warn "mongo not ready"; FAILED+=(mongo); }
  $RUNTIME exec "${PREFIX}-mongo" mongosh --quiet --eval 'rs.initiate()' >/dev/null 2>&1 || true
  wait_for 15 $RUNTIME exec "${PREFIX}-mongo" mongosh --quiet --eval 'rs.status().myState===1 || quit(1)' || warn "mongo replica set not primary yet"
  $RUNTIME exec "${PREFIX}-mongo" mongosh --quiet appdb --eval \
    'db.customers.insertMany([{name:"a"},{name:"b"}]); db.orders.insertOne({customer_id:1});' >/dev/null
  export DATABRIDGE_TEST_MONGO="127.0.0.1,${MONGO_PORT},appdb,,"
  run_test --features mongodb -- discovers_real_mongodb || FAILED+=(mongo)
  unset DATABRIDGE_TEST_MONGO
fi

# ---------- SQL Server ----------
if want mssql; then
  log "mssql/server:2022 (may take ~40s to start)"
  $RUNTIME run -d --name "${PREFIX}-mssql" -e ACCEPT_EULA=Y -e "MSSQL_SA_PASSWORD=${MSSQL_SA_PASS}" \
    -p ${MSSQL_PORT}:1433 mcr.microsoft.com/mssql/server:2022-latest >/dev/null
  SQLCMD=(/opt/mssql-tools18/bin/sqlcmd -S localhost -U sa -P "${MSSQL_SA_PASS}" -C -N)
  wait_for 45 $RUNTIME exec "${PREFIX}-mssql" "${SQLCMD[@]}" -Q "SELECT 1" || { warn "mssql not ready"; FAILED+=(mssql); }
  $RUNTIME exec "${PREFIX}-mssql" "${SQLCMD[@]}" -Q \
    "CREATE DATABASE appdb;" >/dev/null
  $RUNTIME exec "${PREFIX}-mssql" "${SQLCMD[@]}" -d appdb -Q \
    "CREATE TABLE customers(id INT IDENTITY PRIMARY KEY, name NVARCHAR(100)); INSERT INTO customers(name) VALUES ('a'),('b');" >/dev/null
  export DATABRIDGE_TEST_MSSQL="127.0.0.1,${MSSQL_PORT},appdb,sa,${MSSQL_SA_PASS}"
  run_test --features sqlserver -- discovers_real_sqlserver || FAILED+=(mssql)
  unset DATABRIDGE_TEST_MSSQL
fi

echo
if [[ ${#FAILED[@]} -eq 0 ]]; then
  log "all requested connector integration tests passed: ${ENGINES[*]}"
else
  warn "failures: ${FAILED[*]}"; exit 1
fi
