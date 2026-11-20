#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Ensure Secret `atlas-gateway-auth` exists in NAMESPACE with a strong jwt-secret, an optional
# bootstrap-admin-token, and a strong admin-password (replaces the shipped Admin@321 dev default —
# see crates/atlas-common/src/config.rs's admin_password_is_weak()/validate_for_start()).
# Idempotent: never rotates a key that already exists — but an existing Secret from before
# admin-password was added here gets that one key patched in (a missing key would otherwise
# silently fall through to Config's weak dev default and refuse to boot under
# ATLAS_AUTH_REQUIRED=1). Never touches jwt-secret/bootstrap-admin-token on an existing Secret.
#
# Usage (on a host with kubectl):
#   NAMESPACE=zyvor-system bash scripts/ensure-atlas-auth-secret.sh
#   NAMESPACE=rook-ceph    bash scripts/ensure-atlas-auth-secret.sh
#
# Prints newly-generated values to stdout once (so the operator can mint lasting JWTs / log in as
# admin). Re-runs that find every expected key already present print nothing sensitive.
set -euo pipefail

NS="${NAMESPACE:?set NAMESPACE (e.g. zyvor-system or rook-ceph)}"
NAME="${SECRET_NAME:-atlas-gateway-auth}"
KUBECTL="${KUBECTL:-kubectl}"

gen_admin_pass() { openssl rand -base64 24 | tr -d '\n=/+' | head -c 24; }

if $KUBECTL -n "$NS" get secret "$NAME" >/dev/null 2>&1; then
  if $KUBECTL -n "$NS" get secret "$NAME" -o jsonpath='{.data.admin-password}' | grep -q .; then
    echo "secret/${NAME} already exists in ${NS} — left alone" >&2
    exit 0
  fi
  # Pre-existing Secret predates admin-password: patch in just that key, leave jwt-secret and
  # bootstrap-admin-token (and any live sessions signed with them) untouched.
  ADMIN_PASS="$(gen_admin_pass)"
  $KUBECTL -n "$NS" patch secret "$NAME" --type=json \
    -p="[{\"op\":\"add\",\"path\":\"/data/admin-password\",\"value\":\"$(printf '%s' "$ADMIN_PASS" | base64 | tr -d '\n')\"}]" >/dev/null
  echo "secret/${NAME} in ${NS}: added missing admin-password key (jwt-secret/bootstrap-admin-token left alone)" >&2
  echo "  admin-password:           ${ADMIN_PASS}" >&2
  echo "ADMIN_PASSWORD=${ADMIN_PASS}"
  exit 0
fi

JWT="$(openssl rand -base64 48 | tr -d '\n')"
BOOT="$(openssl rand -base64 32 | tr -d '\n=/+' | head -c 40)"
ADMIN_PASS="$(gen_admin_pass)"
$KUBECTL -n "$NS" create secret generic "$NAME" \
  --from-literal=jwt-secret="$JWT" \
  --from-literal=bootstrap-admin-token="$BOOT" \
  --from-literal=admin-password="$ADMIN_PASS" >/dev/null

cat >&2 <<EOF
created secret/${NAME} in ${NS}
  jwt-secret:               (stored in Secret; not printed)
  bootstrap-admin-token:    ${BOOT}
  admin-password:           ${ADMIN_PASS}

Use once to mint lasting JWTs, then remove the bootstrap key:
  curl -sS -H "Authorization: Bearer ${BOOT}" \\
    -H 'Content-Type: application/json' \\
    -d '{"subject":"ops","role":"admin","ttl_secs":86400}' \\
    http://<node>:<port>/api/atlas/v1/auth/tokens

  $KUBECTL -n ${NS} patch secret ${NAME} --type=json \\
    -p='[{"op":"remove","path":"/data/bootstrap-admin-token"}]'
  $KUBECTL -n ${NS} rollout restart deploy/<atlas-gateway>
EOF

# Machine-readable lines for deploy scripts that want to verify with the token / log in as admin.
echo "BOOTSTRAP_ADMIN_TOKEN=${BOOT}"
echo "ADMIN_PASSWORD=${ADMIN_PASS}"
