#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# Builds the atlas-data image (Dockerfile.data) on the lab host and runs deploy/data/gate-smoke.sh there.
# Own remote directory (not ~/.deployment/atlas-ci), so it can run next to a CI run.
# Usage: scripts/atlas-data-image-remote.sh <host> <user>
set -euo pipefail
HOST="${1:?usage: atlas-data-image-remote.sh <host> <user>}"
USER_="${2:?}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR=.deployment/atlas-data-image
ssh "${USER_}@${HOST}" "mkdir -p ~/${DIR}/tools/atlas_data ~/${DIR}/deploy/data"
rsync -az --delete --exclude __pycache__ "${ROOT}/Dockerfile.data" "${ROOT}/.dockerignore" "${USER_}@${HOST}:~/${DIR}/"
rsync -az --delete --exclude __pycache__ "${ROOT}/tools/atlas_data/" "${USER_}@${HOST}:~/${DIR}/tools/atlas_data/"
rsync -az --delete "${ROOT}/deploy/data/gate-smoke.sh" "${USER_}@${HOST}:~/${DIR}/deploy/data/"
ssh "${USER_}@${HOST}" "cd ~/${DIR} && bash deploy/data/gate-smoke.sh"
