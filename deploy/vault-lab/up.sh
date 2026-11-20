#!/usr/bin/env bash
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
# Stand up a throwaway HashiCorp Vault + External Secrets Operator (ESO) in the lab k3s cluster to
# demonstrate the secrets-manager integration pattern a real deployment would use: secrets live in
# Vault, ESO syncs them into a plain Kubernetes Secret via Vault's Kubernetes auth method, and
# Atlas's Deployment references that Secret via the ordinary secretKeyRef it already uses. Dev-mode
# Vault (in-memory, auto-unsealed, root token) — lab-only, mirrors deploy/dex-lab/'s posture.
#
# This demonstrates the pattern against a dedicated demo secret (atlas-demo-gateway-auth), NOT the
# live atlas-gateway-auth Secret the running gateways depend on — switching the real Secret over to
# ESO management is a deliberate follow-up once a real Vault/ESO deployment (not this throwaway lab
# one) is available, not something to wire into a live deployment as a side effect of a demo.
#
# Usage (run ON the lab host, or with kubectl/helm already pointed at it):
#   ./up.sh
#
# What you get:
#   - Namespace vault-lab: Vault (dev mode) via the HashiCorp Helm chart
#   - Namespace external-secrets: the ESO controller via its Helm chart
#   - A Vault KV v2 secret at secret/atlas-demo/gateway-auth (jwt-secret, admin-password)
#   - Vault's Kubernetes auth method, bound to the external-secrets ServiceAccount via a
#     read-only policy scoped to secret/data/atlas-demo/*
#   - A SecretStore + ExternalSecret that syncs it into K8s Secret
#     external-secrets/atlas-demo-gateway-auth, refreshed every 30s
#
# Verify rotation works end-to-end:
#   kubectl exec -n vault-lab vault-0 -- env VAULT_TOKEN=atlas-lab-root-token \
#     vault kv put secret/atlas-demo/gateway-auth jwt-secret=<same> admin-password=<new>
#   # within ~30s:
#   kubectl -n external-secrets get secret atlas-demo-gateway-auth -o jsonpath='{.data.admin-password}' | base64 -d
set -euo pipefail
cd "$(dirname "$0")"

log() { printf '\033[1;36m==> %s\033[0m\n' "$*"; }

log "helm repos"
helm repo add hashicorp https://helm.releases.hashicorp.com >/dev/null 2>&1 || true
helm repo add external-secrets https://charts.external-secrets.io >/dev/null 2>&1 || true
helm repo update >/dev/null

log "Vault (dev mode) in namespace vault-lab"
kubectl create namespace vault-lab --dry-run=client -o yaml | kubectl apply -f - >/dev/null
helm upgrade --install vault hashicorp/vault -n vault-lab \
  --set='server.dev.enabled=true' \
  --set='server.dev.devRootToken=atlas-lab-root-token' \
  --set='injector.enabled=false' \
  --set='server.resources.requests.cpu=50m' \
  --set='server.resources.requests.memory=128Mi' \
  --set='server.resources.limits.cpu=250m' \
  --set='server.resources.limits.memory=256Mi' \
  --wait --timeout=180s

log "External Secrets Operator in namespace external-secrets"
helm upgrade --install external-secrets external-secrets/external-secrets -n external-secrets --create-namespace \
  --set='resources.requests.cpu=25m' --set='resources.requests.memory=64Mi' \
  --set='resources.limits.cpu=100m' --set='resources.limits.memory=128Mi' \
  --wait --timeout=180s

VAULT_EXEC=(kubectl exec -n vault-lab vault-0 -- env VAULT_TOKEN=atlas-lab-root-token vault)

log "KV v2 engine + demo secret"
"${VAULT_EXEC[@]}" secrets enable -path=secret kv-v2 >/dev/null 2>&1 || echo "    secret/ already enabled"
if "${VAULT_EXEC[@]}" kv get secret/atlas-demo/gateway-auth >/dev/null 2>&1; then
  echo "    secret/atlas-demo/gateway-auth already exists — left alone"
else
  DEMO_JWT="$(openssl rand -base64 48 | tr -d '\n')"
  DEMO_PASS="$(openssl rand -base64 24 | tr -d '\n=/+' | head -c 24)"
  "${VAULT_EXEC[@]}" kv put secret/atlas-demo/gateway-auth jwt-secret="$DEMO_JWT" admin-password="$DEMO_PASS" >/dev/null
  echo "    wrote secret/atlas-demo/gateway-auth"
fi

log "Kubernetes auth method + policy + role for the external-secrets ServiceAccount"
"${VAULT_EXEC[@]}" auth enable kubernetes >/dev/null 2>&1 || echo "    kubernetes auth already enabled"
"${VAULT_EXEC[@]}" write auth/kubernetes/config kubernetes_host="https://kubernetes.default.svc:443" >/dev/null
"${VAULT_EXEC[@]}" policy write atlas-demo-read - >/dev/null <<'POLICY'
path "secret/data/atlas-demo/*" {
  capabilities = ["read"]
}
POLICY
"${VAULT_EXEC[@]}" write auth/kubernetes/role/atlas-demo-reader \
  bound_service_account_names=external-secrets \
  bound_service_account_namespaces=external-secrets \
  policies=atlas-demo-read \
  ttl=1h >/dev/null

log "SecretStore + ExternalSecret (syncs into external-secrets/atlas-demo-gateway-auth)"
kubectl apply -f secretstore.yaml
kubectl apply -f externalsecret.yaml

log "waiting for first sync"
for _ in $(seq 1 15); do
  if kubectl -n external-secrets get secret atlas-demo-gateway-auth >/dev/null 2>&1; then
    break
  fi
  sleep 2
done
kubectl -n external-secrets get externalsecret atlas-demo-gateway-auth

cat <<'EOF'

Vault + External Secrets Operator are up.
  Vault UI/API (in-cluster only): http://vault.vault-lab.svc:8200  (root token: atlas-lab-root-token)
  Demo secret path:  secret/atlas-demo/gateway-auth
  Synced K8s Secret: external-secrets/atlas-demo-gateway-auth (keys: jwt-secret, admin-password)

This demo secret is intentionally separate from the live atlas-gateway-auth Secret. To adopt this
pattern for real: point a SecretStore at your bank's actual Vault, write the real secret values
there (jwt-secret, admin-password, oidc-client-secret, state-backup-access-key/secret-key — see
deploy/k8s/atlas-auth-secret.example.yaml for the full key list), and target the ExternalSecret at
atlas-gateway-auth in zyvor-system/rook-ceph instead of this demo Secret.
EOF
