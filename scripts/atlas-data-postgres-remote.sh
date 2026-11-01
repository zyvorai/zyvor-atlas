#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# Runs the atlas_data test suite against a real throwaway Postgres on the lab host (podman): the same tests
# that run on SQLite in CI, with ATLAS_DATA_DATABASE_URL pointing at Postgres. Uses its own remote directory,
# so it does not touch ~/.deployment/atlas-ci.
#
# Usage: scripts/atlas-data-postgres-remote.sh <host> <user>
set -euo pipefail
HOST="${1:?usage: atlas-data-postgres-remote.sh <host> <user>}"
USER_="${2:?}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR=.deployment/atlas-data-pg
log() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }

log "rsync tools/atlas_data -> ${USER_}@${HOST}:~/${DIR}"
ssh "${USER_}@${HOST}" "mkdir -p ~/${DIR}"
rsync -az --delete --exclude __pycache__ "${ROOT}/tools/" "${USER_}@${HOST}:~/${DIR}/tools/"

log "postgres:16-alpine + python test run"
ssh "${USER_}@${HOST}" "cd ~/${DIR} && set -e
  PORT=\$((20000 + RANDOM % 20000)); NAME=atlas-data-pg-\$\$
  PW=\$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
  podman run -d --rm --name \$NAME -e POSTGRES_PASSWORD=\$PW -p 127.0.0.1:\$PORT:5432 postgres:16-alpine >/dev/null
  trap 'podman rm -f \$NAME >/dev/null 2>&1' EXIT
  for _ in \$(seq 1 60); do podman exec \$NAME pg_isready -U postgres >/dev/null 2>&1 && break; sleep 1; done
  sleep 2
  podman run --rm --network=host -v \$PWD/tools:/w/tools:ro -w /w \
    -e ATLAS_DATA_DATABASE_URL=postgresql://postgres:\$PW@127.0.0.1:\$PORT/postgres -e PYTHONPATH=/w/tools -e PYTHONDONTWRITEBYTECODE=1 \
    python:3.12-slim bash -c \"pip install -q 'psycopg[binary]' 2>&1 | tail -1; python3 -m unittest discover -s tools/atlas_data/tests -v 2>&1 | tail -80; echo PY_EXIT=\\\${PIPESTATUS[0]}\"
"
