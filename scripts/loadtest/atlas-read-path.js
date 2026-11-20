// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//
// Load test for Atlas's read-heavy REST surface — approximates the Storage Center console's own
// traffic pattern (per crates/atlas-gateway/ui/src/api/hooks.ts: every dashboard panel polls its
// endpoint on its own interval, typically every 4-15s) rather than a synthetic worst-case, so a
// passing run means "the console stays responsive under N concurrent operators watching
// dashboards," which is the actual thing ATLAS_RATE_LIMIT_RPM was sized against (see
// deploy/k8s/atlas-gateway.yaml's comment on that var).
//
// Usage (see scripts/loadtest/README.md for the full walkthrough):
//   ATLAS_BASE_URL=http://127.0.0.1:5110 k6 run scripts/loadtest/atlas-read-path.js
//
// Only GET requests against read-only endpoints — safe to run repeatedly against the same
// instance (fake driver or real), never mutates state. A separate, opt-in write-path scenario
// (scripts/loadtest/atlas-write-path.js) covers the async job/create path.

import http from "k6/http";
import { check, sleep } from "k6";
import { Rate, Trend } from "k6/metrics";

const BASE_URL = __ENV.ATLAS_BASE_URL || "http://127.0.0.1:5110";
const TOKEN = __ENV.ATLAS_TOKEN || "";

const errorRate = new Rate("atlas_errors");
const requestDuration = new Trend("atlas_request_duration", true);

export const options = {
  scenarios: {
    dashboard_polling: {
      executor: "ramping-vus",
      startVUs: 0,
      stages: [
        { duration: "15s", target: __ENV.ATLAS_LOAD_VUS ? Number(__ENV.ATLAS_LOAD_VUS) : 20 },
        { duration: "30s", target: __ENV.ATLAS_LOAD_VUS ? Number(__ENV.ATLAS_LOAD_VUS) : 20 },
        { duration: "10s", target: 0 },
      ],
    },
  },
  thresholds: {
    // p95 under 500ms and <1% errors is the bar — tune once real production traffic patterns are
    // known (same "tune based on real usage" caveat as ATLAS_RATE_LIMIT_RPM's own default).
    http_req_duration: ["p(95)<500"],
    atlas_errors: ["rate<0.01"],
  },
};

function authHeaders() {
  return TOKEN ? { Authorization: `Bearer ${TOKEN}` } : {};
}

// Weighted like the console's actual panel mix (Overview hits summary/clusters/alerts every ~8s;
// list pages hit their one resource on their own poll). Endpoints and payload shapes match
// docs/API.md.
const ENDPOINTS = [
  { path: "/health", weight: 5 },
  { path: "/api/atlas/v1/metrics/summary", weight: 10 },
  { path: "/api/atlas/v1/clusters", weight: 8 },
  { path: "/api/atlas/v1/pools", weight: 6 },
  { path: "/api/atlas/v1/volumes", weight: 8 },
  { path: "/api/atlas/v1/alerts", weight: 6 },
  { path: "/api/atlas/v1/jobs", weight: 4 },
  { path: "/api/atlas/v1/backends/summary", weight: 3 },
];
const WEIGHTED = ENDPOINTS.flatMap((e) => Array(e.weight).fill(e.path));

export default function () {
  const path = WEIGHTED[Math.floor(Math.random() * WEIGHTED.length)];
  const res = http.get(`${BASE_URL}${path}`, { headers: authHeaders() });
  requestDuration.add(res.timings.duration);
  const ok = check(res, {
    "status is 200": (r) => r.status === 200,
  });
  errorRate.add(!ok);
  sleep(Math.random() * 0.5 + 0.1); // 100-600ms between requests per VU, like a real dashboard tick
}
