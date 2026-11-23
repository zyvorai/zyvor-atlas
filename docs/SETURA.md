<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# Setura front door

Atlas can be fronted by [Setura](https://github.com/zyvorai/zyvor-setura-gateway), a Gateway API
controller with a built-in proxy, through the Kubernetes [Gateway API](https://gateway-api.sigs.k8s.io/).
The Helm chart (`deploy/helm/atlas`) renders a `Gateway` and `HTTPRoute`;
no proxy-specific annotations are involved, so the same chart works with any conformant
implementation. The classic `Ingress` option remains, and the two are mutually exclusive.

## Install the controller

`scripts/deploy-remote.sh` does this for you: step 4d runs `deploy/setura-lab/up.sh --if-missing` on the
target host, which installs Setura only when the cluster has no Setura controller yet
(a GatewayClass with controller `zyvor.dev/setura-gateway`) and does nothing on re-runs. The host needs the
`setura` CLI on PATH. Pass `--no-gateway` to skip the step, and `--rollback` never runs it. By hand:

```bash
deploy/setura-lab/up.sh               # Setura 0.7.0 + the `setura` GatewayClass (additive)
deploy/setura-lab/up.sh --if-missing  # same, but a no-op when Setura is already there
deploy/setura-lab/up.sh --dry-run     # print the manifests only
```

which is the same as:

```bash
setura gateway install --image ghcr.io/zyvorai/zyvor-setura-gateway:0.7.0 --namespace setura-system | kubectl apply -f -
```

This creates the `setura-system` namespace and one controller Deployment (plus the standard Gateway API
CRDs when the cluster has none). Existing Ingress objects and workloads are untouched.

## Enable it in the chart

```bash
helm upgrade --install atlas deploy/helm/atlas \
  --set gateway.enabled=true --set gateway.host=atlas.example.com \
  --set gateway.tlsSecretName=atlas-tls      # optional: HTTPS :7443 + HTTP :80 -> 301 redirect
```

| Value | Default | Meaning |
|---|---|---|
| `gateway.enabled` | `false` | Render `Gateway` + `HTTPRoute`. Fails if `ingress.enabled` is also set. |
| `gateway.host` | `""` | Required hostname. |
| `gateway.createGateway` | `true` | `false` attaches only an `HTTPRoute` to `gateway.gatewayName` / `gateway.gatewayNamespace` (that Gateway must allow routes from this namespace). |
| `gateway.gatewayClassName` | `setura` | GatewayClass for the created Gateway. |
| `gateway.tlsSecretName` | `""` | `kubernetes.io/tls` Secret; adds the HTTPS listener and an HTTP redirect route. |
| `gateway.httpsPort` | `7443` | Port of the HTTPS listener (not 443); the HTTP→HTTPS redirect names it. |
| `gateway.requestTimeout` | `0s` | HTTPRoute request timeout; `0s` keeps the streamable-HTTP MCP endpoint's long-lived sessions open. |

Upgrading from the earlier Envoy Gateway setup: the values moved from `envoy.*` to `gateway.*` and the default
GatewayClass from `eg` to `setura`.

## Lab notes

- **Listener ports.** Setura's generated Service exposes ports 80 and 443. The default HTTPS listener is
  `7443`, so install the controller with `setura gateway install --expose-gateway-ports` (the controller then
  adds a Service port for every listener port) or add the port to the `setura-gateway` Service yourself.
- **Gateway address.** The Gateway's `status.addresses` come from the `setura-gateway` Service: its
  LoadBalancer ingress, else its ClusterIP. A bare k3s without svclb/MetalLB has no LoadBalancer address, so
  reach it on the Service's NodePort (`kubectl -n setura-system get svc setura-gateway`) or by port-forward.
- **Gateway API CRDs.** Setura does not ship them; `up.sh` applies the standard channel
  (`GATEWAY_API_VERSION`, default v1.4.0) when `gateways.gateway.networking.k8s.io` is missing.
- **Disk pressure.** The install pulls an image; a node below the kubelet's free-space threshold evicts
  pods. Keep the root filesystem under ~90% (see `scripts/ci-remote.sh` for where the CI
  cache lives).

## Scope and limits

- Routes plain HTTP to `service.httpPort` (the REST API, console and `/api/atlas/v1/mcp`). gRPC
  (h2c) and TLS-to-backend (the gateway's own `:5443`) are not routed by this chart.
- The agent API (`atlas-io-agent`) stays cluster-internal; do not publish it through this route.
- Certificates are yours to supply (for example with cert-manager); the chart only references a Secret.
- Moving other products off nginx is done in their own repositories/manifests, not here.
