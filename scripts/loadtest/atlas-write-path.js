// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
//
// Opt-in write-path load test: each iteration creates a volume (POST /volumes, 202 + job_id),
// polls the job to a terminal state, then deletes the volume. Separate from
// atlas-read-path.js (which is read-only and safe to run repeatedly) because this one mutates
// state — point it at a throwaway/fake-driver instance, not a shared or production gateway.
//
// Usage:
//   ATLAS_BASE_URL=http://127.0.0.1:5110 k6 run scripts/loadtest/atlas-write-path.js

import http from "k6/http";
import { check, sleep } from "k6";
import { Rate, Trend } from "k6/metrics";

const BASE_URL = __ENV.ATLAS_BASE_URL || "http://127.0.0.1:5110";
const TOKEN = __ENV.ATLAS_TOKEN || "";

const errorRate = new Rate("atlas_write_errors");
const jobLatency = new Trend("atlas_job_completion_ms", true);

export const options = {
  scenarios: {
    volume_lifecycle: {
      executor: "ramping-vus",
      startVUs: 0,
      stages: [
        { duration: "10s", target: __ENV.ATLAS_LOAD_VUS ? Number(__ENV.ATLAS_LOAD_VUS) : 5 },
        { duration: "20s", target: __ENV.ATLAS_LOAD_VUS ? Number(__ENV.ATLAS_LOAD_VUS) : 5 },
        { duration: "5s", target: 0 },
      ],
    },
  },
  thresholds: {
    atlas_write_errors: ["rate<0.02"],
  },
};

function authHeaders() {
  return TOKEN
    ? { Authorization: `Bearer ${TOKEN}`, "Content-Type": "application/json" }
    : { "Content-Type": "application/json" };
}

export default function () {
  const name = `loadtest-${__VU}-${__ITER}-${Date.now()}`;
  const createStart = Date.now();

  const createRes = http.post(
    `${BASE_URL}/api/atlas/v1/volumes`,
    JSON.stringify({
      tenant_id: "loadtest",
      name,
      size_bytes: 1073741824,
      kind: "block",
      policy: "database",
    }),
    { headers: authHeaders() },
  );
  const created = check(createRes, {
    "create accepted (202)": (r) => r.status === 202,
  });
  errorRate.add(!created);
  if (!created) {
    sleep(1);
    return;
  }

  // The create response already carries the (idempotent, deterministic) volume id — no need to
  // poll the job just to find out what to clean up.
  const volumeId = createRes.json("resource.volume_id");
  const jobId = createRes.json("job_id");

  let terminal = false;
  for (let i = 0; i < 20 && !terminal; i++) {
    sleep(0.25);
    const state = http
      .get(`${BASE_URL}/api/atlas/v1/jobs/${jobId}`, { headers: authHeaders() })
      .json("state");
    terminal = state === "succeeded" || state === "failed";
  }
  jobLatency.add(Date.now() - createStart);
  errorRate.add(!terminal);

  // Cleanup: 202 (deleted, enqueued) and 404 (create job failed before persisting — e.g. no live
  // k8s cluster attached in this environment) are both a clean end state; only an unexpected
  // status counts as a script/environment problem worth failing the run over.
  if (volumeId) {
    const delRes = http.del(`${BASE_URL}/api/atlas/v1/volumes/${volumeId}`, null, {
      headers: authHeaders(),
    });
    errorRate.add(![202, 404].includes(delRes.status));
  }

  sleep(Math.random() * 0.5);
}
