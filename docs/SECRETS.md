<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Secrets backend: env vars or Vault

By default (`ATLAS_SECRETS_BACKEND` unset, or `env`) Atlas reads `jwt_secret`/`admin_password`/
OIDC `client_secret` the way it always has — plain env vars, typically populated from a K8s
`Secret` via `secretKeyRef` (`deploy/k8s/atlas-gateway*.yaml`). `deploy/vault-lab/` already
documents a K8s-layer pattern for this (Vault → External Secrets Operator → K8s `Secret` → Atlas's
existing `secretKeyRef`) — that pattern needs no Atlas-side changes at all.

This page covers the other option: Atlas fetching these secrets **directly** from Vault at
startup, for deployments that don't run (or don't want to depend on) External Secrets Operator.

## Enabling it

```bash
ATLAS_SECRETS_BACKEND=vault
ATLAS_VAULT_ADDR=https://vault.example.com:8200
ATLAS_VAULT_TOKEN=<a Vault token with read access to the path below>
ATLAS_VAULT_SECRET_PATH=secret/data/atlas/gateway-auth   # KV v2 — note the "data/" segment
```

At startup (before the weak-secret validation that would otherwise refuse to boot), Atlas issues
one `GET {ATLAS_VAULT_ADDR}/v1/{ATLAS_VAULT_SECRET_PATH}` with `X-Vault-Token: <token>` and, from
the KV v2 response's `data.data` object, overwrites:

| Vault key | Overwrites |
|---|---|
| `jwt-secret` | `ATLAS_JWT_SECRET` |
| `admin-password` | `ATLAS_ADMIN_PASSWORD` |
| `oidc-client-secret` | the configured OIDC provider's client secret (only if OIDC is already configured via `ATLAS_OIDC_*`) |

These are the same key names `deploy/k8s/atlas-auth-secret.example.yaml` already uses for the K8s
`Secret` — so the same Vault secret can back either integration path.

If the Vault request fails for any reason (unreachable, wrong token, path isn't KV v2, etc.),
**startup fails** — unlike the OIDC-discovery-at-boot failure mode (which just disables SSO and
keeps going), a secrets backend that's supposed to provide the JWT signing key but can't reach
Vault must not silently fall back to whatever `ATLAS_JWT_SECRET` happens to be set to (the dev
default, most likely).

## What this doesn't cover

- **Auth method**: token auth only (`X-Vault-Token`). AppRole or Kubernetes auth would be more
  production-grade (no long-lived token to manage) but are a larger follow-up — this is a first
  slice, not full Vault integration.
- **Secret rotation**: this is a one-shot fetch at startup, not a live-reload. Rotating the secret
  in Vault means restarting the gateway to pick it up (same as rotating a K8s `Secret` today,
  which also needs a pod restart to take effect unless something separately signals the process).
- **Other credentials**: DataBridge/RGW S3 credentials already use a different, already-working
  reference-based pattern (`secret_ref` on bucket/source records, resolved against a live K8s
  `Secret` per-call — see `crates/atlas-gateway/src/routes/object_store.rs`) — this page is
  specifically about the JWT/admin-password/OIDC secrets that were env-var-only before.

## Helm chart

`deploy/helm/atlas/values.yaml`'s `secretsBackend.*` block wires all of this, referencing a Secret
for the Vault token (never inlining it):

```yaml
secretsBackend:
  kind: vault
  vault:
    addr: "https://vault.example.com:8200"
    tokenSecretName: atlas-vault-token
    secretPath: "secret/data/atlas/gateway-auth"
```
