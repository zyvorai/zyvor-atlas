// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { describe, expect, it } from "vitest";
import type { MetricHistoryPoint, MetricsSummary } from "../api/types";

function summary(read: number, write: number, usedPct = 10): MetricsSummary {
  return {
    raw_capacity_bytes: 1e12,
    used_capacity_bytes: 1e11,
    available_capacity_bytes: 9e11,
    used_capacity_percent: usedPct,
    clusters: 1,
    pools: 1,
    volumes: 0,
    snapshots: 0,
    buckets: 0,
    backups: 0,
    client_io: {
      read_ops_total: read,
      write_ops_total: write,
      read_bytes_total: 0,
      write_bytes_total: 0,
    },
    recovery: {
      pg_recovering: 0,
      pg_backfilling: 0,
      objects_degraded: 0,
      objects_misplaced: 0,
      objects_unfound: 0,
    },
  };
}

describe("history buffer", () => {
  it("computes IOPS deltas and seeds only when empty", async () => {
    // Fresh module instance so prior suite imports don't share the rolling buffer.
    const mod = await import("./history?t=" + Date.now());
    const points: MetricHistoryPoint[] = [
      {
        ts: new Date(Date.now() - 2000).toISOString(),
        used_capacity_bytes: 100,
        raw_capacity_bytes: 1000,
        volumes: 0,
        snapshots: 0,
        read_bytes: 0,
        write_bytes: 0,
        read_ops: 10,
        write_ops: 5,
        jobs_running: 0,
        alerts_open: 0,
      },
      {
        ts: new Date(Date.now() - 1000).toISOString(),
        used_capacity_bytes: 200,
        raw_capacity_bytes: 1000,
        volumes: 0,
        snapshots: 0,
        read_bytes: 0,
        write_bytes: 0,
        read_ops: 30,
        write_ops: 15,
        jobs_running: 0,
        alerts_open: 0,
      },
    ];
    mod.seed(points);
    expect(mod.history()).toHaveLength(2);
    expect(mod.history()[1].usedPct).toBe(20);

    // Second seed must not clobber live/seeded samples.
    mod.seed([
      {
        ...points[0],
        used_capacity_bytes: 999,
      },
    ]);
    expect(mod.history()[0].usedBytes).toBe(100);

    mod.recordSummary(summary(100, 50, 21));
    await new Promise((r) => setTimeout(r, 25));
    mod.recordSummary(summary(200, 90, 22));
    const last = mod.history().at(-1)!;
    expect(last.usedPct).toBe(22);
    expect(last.readIops).toBeGreaterThan(0);
    expect(last.writeIops).toBeGreaterThan(0);
  });
});
