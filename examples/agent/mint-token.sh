#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: Apache-2.0
# Mint a short-lived OPERATOR token for an AI agent / MCP client and print only the token.
#
#   ATLAS_BASE_URL=http://127.0.0.1:5110 ATLAS_ADMIN_TOKEN=<admin jwt> \
#     examples/agent/mint-token.sh [subject] [ttl_secs]
#
# Deliberately never mints an admin token. Default TTL is 7 days; the server clamps to 60s..90d.
# Revoke early with: POST $ATLAS_BASE_URL/api/atlas/v1/auth/tokens/<jti>/revoke  (jti is printed
# to stderr). The admin token is read from the environment only, never from argv.
set -euo pipefail
: "${ATLAS_BASE_URL:?set ATLAS_BASE_URL (e.g. http://127.0.0.1:5110)}"
: "${ATLAS_ADMIN_TOKEN:?set ATLAS_ADMIN_TOKEN (an admin JWT)}"
SUBJECT="${1:-ops-agent}"
TTL="${2:-604800}"
[[ "$TTL" =~ ^[0-9]+$ ]] || { echo "ttl_secs must be an integer" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }

resp="$(curl -fsS -X POST "${ATLAS_BASE_URL%/}/api/atlas/v1/auth/tokens" \
  -H "Authorization: Bearer ${ATLAS_ADMIN_TOKEN}" \
  -H 'Content-Type: application/json' \
  -d "$(jq -nc --arg s "$SUBJECT" --argjson t "$TTL" '{subject:$s, role:"operator", ttl_secs:$t}')")"
echo "minted operator token for '${SUBJECT}', jti=$(jq -r .jti <<<"$resp"), expires_at=$(jq -r .expires_at <<<"$resp")" >&2
jq -r .token <<<"$resp"
