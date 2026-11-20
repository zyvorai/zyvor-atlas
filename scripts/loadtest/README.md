<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Load/performance testing

Atlas had no load testing at all before this — no k6/vegeta/locust script, no perf regression
gate, not even a `cargo bench`. Two [k6](https://k6.io) scripts, run against a live gateway:

- **`atlas-read-path.js`** — read-only, safe to run repeatedly against any instance (fake or real
  driver). Approximates the Storage Center console's own dashboard-polling traffic pattern (see
  the script's header comment) rather than a synthetic worst case.
- **`atlas-write-path.js`** — opt-in, mutates state (creates/deletes volumes in a loop). Point it
  at a throwaway/fake-driver instance, never a shared or production gateway.

## Running locally

```bash
brew install k6   # or see https://k6.io/docs/get-started/installation/

make run &         # or: cargo run -p atlas-gateway
ATLAS_BASE_URL=http://127.0.0.1:5110 k6 run scripts/loadtest/atlas-read-path.js

# Optional: the write-path scenario (creates real volumes against the fake driver)
ATLAS_BASE_URL=http://127.0.0.1:5110 k6 run scripts/loadtest/atlas-write-path.js
```

`ATLAS_LOAD_VUS` (default 20 for read-path, 5 for write-path) controls concurrency;
`ATLAS_TOKEN` sets a Bearer token if the target has `ATLAS_AUTH_REQUIRED=1`.

Note: the write-path scenario's job-completion checks only pass end-to-end (state `succeeded`,
not `failed`) against a gateway with a live Kubernetes cluster attached — the fake *storage*
driver alone still needs a real k8s API to create the PVC the volume is backed by. Run it against
`deploy/rook-ceph-lab/` (or any cluster with `--kubeconfig` reachable) for the full happy path;
against a bare `cargo run` with no cluster, jobs correctly land in `failed` and the script's own
error accounting treats that — and the resulting cleanup 404 — as an expected outcome, not a bug.

## CI

`.github/workflows/load-test.yml` runs both scenarios nightly (`schedule:`) against a fresh
fake-driver gateway in the runner, and can be triggered on demand (`workflow_dispatch`) — not on
every PR, since load tests are slower and noisier than the rest of the CI suite. Results (k6's
summary + thresholds pass/fail) are uploaded as a build artifact.
