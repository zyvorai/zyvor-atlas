<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Dex OIDC lab

Throwaway [Dex](https://dexidp.io/) identity provider for testing Atlas's OIDC/SSO login
(`crates/atlas-gateway/src/routes/oidc.rs`, `ATLAS_OIDC_*` env vars). Plain HTTP, in-memory
storage, static test users — **for the lab, not for production**, same posture as
`deploy/rook-ceph-lab/`.

## Install

```
./up.sh
```

Idempotent — rerun any time; the OAuth2 client secret is generated once (stored in the
`dex-oidc-client` Secret) and reused on subsequent runs, so redeploying Dex doesn't invalidate an
already-configured Atlas gateway.

## What you get

| Object | Purpose |
|---|---|
| `Namespace zyvor-system` | created if missing (same namespace as the fake/k8s `atlas-gateway`) |
| `ConfigMap dex-config` | rendered `config.yaml` — issuer, static client, static test users |
| `Secret dex-oidc-client` | the generated OAuth2 client secret shared with Atlas |
| `Deployment/Service dex` | single replica, NodePort 30556 |

Static test users (email / password / group → Atlas role):

| Email | Password | Dex group | Atlas role |
|---|---|---|---|
| alice@zyvor.lab | `AlicePass123!` | `admin` | admin |
| bob@zyvor.lab | `BobPass123!` | `operator` | operator |
| carol@zyvor.lab | `CarolPass123!` | *(none)* | viewer (default) |

## Wire it into Atlas

`up.sh` prints the exact values at the end. Set them as env vars on the `atlas-gateway`
Deployment (namespace `zyvor-system`) and redeploy:

```
ATLAS_OIDC_ISSUER_URL=http://<node-ip>:30556
ATLAS_OIDC_CLIENT_ID=atlas-console
ATLAS_OIDC_CLIENT_SECRET=<printed by up.sh>
ATLAS_OIDC_REDIRECT_URL=http://<node-ip>:30510/api/atlas/v1/auth/oidc/callback
ATLAS_OIDC_ADMIN_GROUP=admin
ATLAS_OIDC_OPERATOR_GROUP=operator
```

## Verify

```
curl -s http://<node-ip>:30556/.well-known/openid-configuration | head
curl -s http://<node-ip>:30510/api/atlas/v1/auth/oidc/status   # {"enabled":true} once Atlas has the env set
```

Then open `http://<node-ip>:30510/`, click **Sign in with SSO**, and log in as one of the test
users above.

## MCP access tokens from Dex (manual check, not run)

Atlas can accept a JWT access token issued by this Dex on the MCP endpoint (`ATLAS_MCP_IDP_ISSUER`,
[`docs/MCP.md`](../../docs/MCP.md#idp-access-tokens-resource-server)). It is separate from the console
login above and from the `atlas-console` client. **These steps were written from Dex's documentation and
the Atlas validation rules and have never been run against a real Dex**; the only automated coverage is
an in-process fake JWKS server. If a step differs on your Dex version, trust what the token actually
contains over this page.

Two Dex facts decide how this works. Dex has no RFC 8707 `resource` parameter, so it cannot be told to
put the MCP URL in `aud`; the `aud` of a Dex token is the client id it was issued to. So register a
client **whose id is the MCP endpoint URL**. And Atlas only accepts JWT access tokens, so check the
`access_token` Dex returns really is one (three dot-separated segments) before going further.

1. **Pick the URLs.** With `<node-ip>` as above, the audience is `http://<node-ip>:30510/api/atlas/v1/mcp`.
   Add a client to the `staticClients` list of the `dex-config` ConfigMap (public client, so no secret;
   `up.sh` renders the file, so apply this by editing the ConfigMap, then `kubectl -n zyvor-system rollout restart deploy/dex`):

   ```yaml
   - id: http://<node-ip>:30510/api/atlas/v1/mcp
     name: 'Atlas MCP'
     public: true
     redirectURIs:
       - http://localhost:8085/callback
   ```

2. **Configure Atlas** (env on the `atlas-gateway` Deployment, then redeploy). `ATLAS_MCP_IDP_ALLOW_HTTP=1`
   is needed only because this lab Dex is plain HTTP on a non-loopback address:

   ```
   ATLAS_PUBLIC_URL=http://<node-ip>:30510
   ATLAS_MCP_IDP_ISSUER=http://<node-ip>:30556
   ATLAS_MCP_IDP_ALLOW_HTTP=1
   ATLAS_OIDC_ADMIN_GROUP=admin
   ATLAS_OIDC_OPERATOR_GROUP=operator
   ATLAS_AUTH_REQUIRED=1
   ```

   The gateway refuses to start if `ATLAS_MCP_IDP_ISSUER` is set without `ATLAS_PUBLIC_URL`. It does not
   contact Dex at startup; the first IdP token triggers discovery and a JWKS fetch.
   `curl -s http://<node-ip>:30510/.well-known/oauth-protected-resource/api/atlas/v1/mcp` should now list
   `"authorization_servers": ["http://<node-ip>:30556"]`.

3. **Get a token** with the authorization-code flow and PKCE (no password grant in this Dex). Generate a verifier and
   challenge, open the URL in a browser, log in as `alice@zyvor.lab` (or `bob`, `carol`), then copy `code`
   from the address bar of the failed redirect to `localhost:8085`:

   ```
   AUD="http://<node-ip>:30510/api/atlas/v1/mcp"
   VERIFIER=$(openssl rand -base64 48 | tr -d '=+/\n' | cut -c1-64)
   CHALLENGE=$(printf %s "$VERIFIER" | openssl dgst -sha256 -binary | openssl base64 -A | tr '+/' '-_' | tr -d '=')
   echo "http://<node-ip>:30556/auth?client_id=$AUD&redirect_uri=http://localhost:8085/callback&response_type=code&scope=openid%20email%20groups&state=x&code_challenge=$CHALLENGE&code_challenge_method=S256"
   # after logging in, paste the code:
   TOKEN=$(curl -s http://<node-ip>:30556/token -d grant_type=authorization_code -d code=<CODE> \
     -d redirect_uri=http://localhost:8085/callback -d client_id="$AUD" -d code_verifier="$VERIFIER" | jq -r .access_token)
   echo "$TOKEN" | cut -d. -f2 | tr '_-' '/+' | base64 -d 2>/dev/null | jq '{iss,aud,sub,exp,groups}'
   ```

   Check the decoded payload: `iss` must equal `ATLAS_MCP_IDP_ISSUER` exactly, `aud` must be the MCP URL, and
   `groups` must contain `admin` or `operator` for alice or bob. If `$TOKEN` is `null` or not a JWT, or `groups` is
   missing, stop: Atlas cannot use that token.

4. **Call the MCP endpoint:**

   ```
   curl -si http://<node-ip>:30510/api/atlas/v1/mcp -H "authorization: Bearer $TOKEN" \
     -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
     -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"curl","version":"1"}}}'
   ```

   Expect `200`. Then confirm the identity: the audit trail (`GET /api/atlas/v1/audit?actor=idp-` with an
   Atlas admin token) should show `mcp.tool.call` rows for `idp-<8 hex>:<Dex user id>` once you have called a
   tool, with the role from the groups (alice admin, bob operator, carol viewer, who is refused operator tools).

5. **Negative checks.** The same token against `GET /api/atlas/v1/clusters` or `/metrics` must be `401`. A token
   from the `atlas-console` client (its `aud` is `atlas-console`) against `/mcp` must be `401` with
   `error="invalid_token"` (the body says the audience does not match). Stop Dex and call with a token that
   has already been validated: it keeps working for up to an hour (cached keys); a never-seen key id gets a `401`
   and a log line `identity provider keys could not be fetched`.

## Teardown

```
kubectl -n zyvor-system delete deploy/dex svc/dex configmap/dex-config secret/dex-oidc-client
```

(No `teardown.sh` — this is a 4-object deploy, not worth the ceremony `deploy/rook-ceph-lab/`
needs for a whole Ceph cluster.)
