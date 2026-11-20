#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Stand up a throwaway Dex OIDC provider in the lab k3s cluster to test Atlas's OIDC/SSO login
# (crates/atlas-gateway/src/routes/oidc.rs). Plain HTTP, in-memory storage, static test users —
# lab-only, mirrors deploy/rook-ceph-lab/README.md's "for the lab, NOT for production" posture.
#
# Usage (run ON the lab host, or with kubectl already pointed at it):
#   ./up.sh
#
# What you get:
#   - Namespace zyvor-system (created if missing, same namespace as the fake/k8s atlas-gateway)
#   - Dex on NodePort 30556, issuer http://<node-ip>:30556
#   - A static OAuth2 client "atlas-console" registered for the atlas-gateway (30510) callback
#   - 3 static test users: alice (group "admin"), bob (group "operator"), carol (no group -> viewer)
#   - A k8s Secret `dex-oidc-client` holding the generated client secret (idempotent: reused on rerun)
#
# After this, wire ATLAS_OIDC_* env vars into the atlas-gateway Deployment (see
# scripts/deploy-remote.sh's manifest / README) using the issuer/client-id/secret printed below,
# then redeploy the gateway.
set -euo pipefail
cd "$(dirname "$0")"
NS=zyvor-system

log() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }

log "namespace"
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -

log "OIDC client secret (random if missing, reused if this is a rerun)"
if kubectl -n "$NS" get secret dex-oidc-client >/dev/null 2>&1; then
  CLIENT_SECRET="$(kubectl -n "$NS" get secret dex-oidc-client -o jsonpath='{.data.client-secret}' | base64 -d)"
  echo "    dex-oidc-client already exists — reusing its secret"
else
  CLIENT_SECRET="$(openssl rand -base64 24 | tr -d '/+=' | head -c 32)"
  kubectl -n "$NS" create secret generic dex-oidc-client --from-literal=client-secret="$CLIENT_SECRET"
  echo "    created dex-oidc-client"
fi

NODE_IP="$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')"
log "rendering Dex config for issuer http://${NODE_IP}:30556"
sed -e "s/DEX_ISSUER_HOST/${NODE_IP}/g" -e "s/DEX_CLIENT_SECRET/${CLIENT_SECRET}/g" dex-config.yaml \
  > /tmp/dex-config.rendered.yaml
kubectl apply -f /tmp/dex-config.rendered.yaml
rm -f /tmp/dex-config.rendered.yaml

log "Dex deployment + service"
kubectl apply -f dex-deployment.yaml
# The config is a ConfigMap volume mount, not env vars — kubectl apply on the same manifest name
# doesn't restart the pod when only the ConfigMap contents changed, so force a restart every run
# (idempotent — a no-op restart if the config genuinely didn't change).
kubectl -n "$NS" rollout restart deploy/dex
kubectl -n "$NS" rollout status deploy/dex --timeout=120s

cat <<EOF

Dex is up:
  Issuer            http://${NODE_IP}:30556
  Client ID         atlas-console
  Client secret     ${CLIENT_SECRET}
  Redirect URI      http://${NODE_IP}:30510/api/atlas/v1/auth/oidc/callback

Test users (email / password / group):
  alice@zyvor.lab / AlicePass123! / admin
  bob@zyvor.lab   / BobPass123!   / operator
  carol@zyvor.lab / CarolPass123! / (none -> viewer)

Next: set on the atlas-gateway Deployment (namespace ${NS}) and redeploy:
  ATLAS_OIDC_ISSUER_URL=http://${NODE_IP}:30556
  ATLAS_OIDC_CLIENT_ID=atlas-console
  ATLAS_OIDC_CLIENT_SECRET=${CLIENT_SECRET}
  ATLAS_OIDC_REDIRECT_URL=http://${NODE_IP}:30510/api/atlas/v1/auth/oidc/callback
  ATLAS_OIDC_ADMIN_GROUP=admin
  ATLAS_OIDC_OPERATOR_GROUP=operator
EOF
