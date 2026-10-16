<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: Apache-2.0 -->
# Envoy front door

Atlas can be fronted by [Envoy](https://github.com/envoyproxy/envoy) through the Kubernetes
[Gateway API](https://gateway-api.sigs.k8s.io/), using [Envoy Gateway](https://github.com/envoyproxy/gateway)
as the controller. The Helm chart (`deploy/helm/atlas`) renders a `Gateway` and `HTTPRoute`;
no proxy-specific annotations are involved, so the same chart works with any conformant
implementation. The classic `Ingress` option remains, and the two are mutually exclusive.

## Install the controller

`scripts/deploy-remote.sh` does this for you: step 4d runs `deploy/envoy-lab/up.sh --if-missing` on the
target host, which installs Envoy Gateway only when the cluster has no Envoy Gateway controller yet
(a GatewayClass served by `gateway.envoyproxy.io`) and does nothing on re-runs. Pass `--no-envoy` to
skip the step, and `--rollback` never runs it. By hand:

```bash
deploy/envoy-lab/up.sh               # Envoy Gateway v1.9.2 + the `eg` GatewayClass (additive)
deploy/envoy-lab/up.sh --if-missing  # same, but a no-op when an Envoy Gateway is already there
deploy/envoy-lab/up.sh --dry-run     # render only
```

This creates the `envoy-gateway-system` namespace, the Gateway API and Envoy Gateway CRDs, and one
controller Deployment. Existing Ingress objects and workloads are untouched.

## Enable it in the chart

```bash
helm upgrade --install atlas deploy/helm/atlas \
  --set envoy.enabled=true --set envoy.host=atlas.example.com \
  --set envoy.tlsSecretName=atlas-tls      # optional: HTTPS :7443 + HTTP :80 -> 301 redirect
```

| Value | Default | Meaning |
|---|---|---|
| `envoy.enabled` | `false` | Render `Gateway` + `HTTPRoute`. Fails if `ingress.enabled` is also set. |
| `envoy.host` | `""` | Required hostname. |
| `envoy.createGateway` | `true` | `false` attaches only an `HTTPRoute` to `envoy.gatewayName` / `envoy.gatewayNamespace` (that Gateway must allow routes from this namespace). |
| `envoy.gatewayClassName` | `eg` | GatewayClass for the created Gateway. |
| `envoy.tlsSecretName` | `""` | `kubernetes.io/tls` Secret; adds the HTTPS listener and an HTTP redirect route. |
| `envoy.httpsPort` | `7443` | Port of the HTTPS listener (not 443); the HTTP→HTTPS redirect names it. |
| `envoy.requestTimeout` | `0s` | HTTPRoute request timeout; `0s` keeps the streamable-HTTP MCP endpoint's long-lived sessions open. |

## Lab notes

- **`Programmed=False` / `AddressNotAssigned`:** Envoy Gateway exposes each Gateway as a
  `LoadBalancer` Service. A bare k3s without svclb/MetalLB never assigns it an address, so the Gateway
  reports `Programmed=False` even though it works: traffic is reachable on the Service's NodePort
  (`kubectl -n envoy-gateway-system get svc`). Install a LoadBalancer implementation (or an
  `EnvoyProxy` with `envoyService.type: NodePort`) for a real address.
- **Jobs are required.** The controller's certgen step is a Helm hook Job. If another component's
  admission webhook (for example Kueue with `failurePolicy: Fail`) is down, all Jobs are rejected and
  the install fails with a webhook error; fix that first.
- **Disk pressure.** The install pulls images; a node below the kubelet's free-space threshold evicts
  the certgen pod. Keep the root filesystem under ~90% (see `scripts/ci-remote.sh` for where the CI
  cache lives).

## Scope and limits

- Routes plain HTTP to `service.httpPort` (the REST API, console and `/api/atlas/v1/mcp`). gRPC
  (h2c) and TLS-to-backend (the gateway's own `:5443`) are not routed by this chart.
- The agent API (`atlas-io-agent`) stays cluster-internal; do not publish it through this route.
- Certificates are yours to supply (for example with cert-manager); the chart only references a Secret.
- Moving other products off nginx is done in their own repositories/manifests, not here.
