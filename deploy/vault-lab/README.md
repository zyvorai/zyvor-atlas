<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Vault + External Secrets Operator lab

Throwaway [HashiCorp Vault](https://www.vaultproject.io/) (dev mode) + [External Secrets
Operator](https://external-secrets.io/) (ESO) to demonstrate the secrets-manager integration
pattern recommended for a real deployment: secrets live in Vault, ESO syncs them into an ordinary
Kubernetes `Secret` via Vault's Kubernetes auth method, and Atlas's Deployment references that
`Secret` through the same `secretKeyRef` it already uses for `atlas-gateway-auth`. In-memory
Vault, auto-unsealed, root token known — **for the lab, not for production**, same posture as
`deploy/dex-lab/`.

This demonstrates the pattern against a dedicated demo secret (`atlas-demo-gateway-auth`), **not**
the live `atlas-gateway-auth` Secret the running gateways depend on. Switching the real Secret
over to ESO management is a deliberate follow-up once a real Vault/ESO deployment is available —
see [Adopting this for real](#adopting-this-for-real) below.

## Install

```
./up.sh
```

Idempotent — rerun any time; the demo secret in Vault is written once and left alone on
subsequent runs.

## What you get

| Object | Purpose |
|---|---|
| `Namespace vault-lab` | Vault (dev mode), via the HashiCorp Helm chart |
| `Namespace external-secrets` | the ESO controller, via its Helm chart |
| `secret/atlas-demo/gateway-auth` (in Vault) | KV v2 secret: `jwt-secret`, `admin-password` |
| Vault Kubernetes auth + `atlas-demo-reader` role | scoped read-only access to that one path, bound to the `external-secrets` ServiceAccount |
| `SecretStore vault-lab` / `ExternalSecret atlas-demo-gateway-auth` (in `external-secrets` ns) | syncs the Vault secret into a K8s `Secret` every 30s |

## Verify the sync — and that rotation actually works

```
# The synced K8s Secret exists and has the two keys:
kubectl -n external-secrets get secret atlas-demo-gateway-auth -o jsonpath='{.data}'

# Rotate the value in Vault...
kubectl exec -n vault-lab vault-0 -- env VAULT_TOKEN=atlas-lab-root-token \
  vault kv put secret/atlas-demo/gateway-auth \
    jwt-secret=<same-as-before> admin-password=<new-value>

# ...and within ~30s the K8s Secret picks it up with no kubectl/redeploy on the Atlas side:
kubectl -n external-secrets get secret atlas-demo-gateway-auth -o jsonpath='{.data.admin-password}' | base64 -d
```

## Adopting this for real

1. Point `secretstore.yaml`'s `spec.provider.vault.server` at the bank's actual Vault instead of
   this lab one (and its real auth method — Kubernetes auth is one option among several Vault
   supports; use whatever the bank's platform team already runs).
2. Write the real secret values under whatever Vault path they choose — `jwt-secret`,
   `admin-password`, `oidc-client-secret`, `state-backup-access-key`/`state-backup-secret-key` (see
   `deploy/k8s/atlas-auth-secret.example.yaml` for the full key list `atlas-gateway-auth` expects).
3. Change `externalsecret.yaml`'s `target.name` from `atlas-demo-gateway-auth` to
   `atlas-gateway-auth`, and apply it in whichever namespace the gateway runs in (`zyvor-system` or
   `rook-ceph`) instead of `external-secrets`.
4. `deploy/k8s/atlas-gateway*.yaml` already reference `atlas-gateway-auth` via `secretKeyRef` —
   nothing on the Atlas side needs to change once the Secret itself is ESO-managed.

## Teardown

```
kubectl -n external-secrets delete externalsecret atlas-demo-gateway-auth
kubectl -n external-secrets delete secretstore -n external-secrets vault-lab
helm uninstall external-secrets -n external-secrets
helm uninstall vault -n vault-lab
kubectl delete namespace vault-lab external-secrets
```
