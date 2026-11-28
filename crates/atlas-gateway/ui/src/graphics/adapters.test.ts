// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { describe, expect, it } from "vitest";
import type { MetricHistoryPoint, MetricsSummary, Osd, StoragePool } from "../api/types";
import {
  capacityFromSummary,
  latencySeries,
  poolUsageFromCephDf,
  poolUsageFromPools,
  replicaSizesFromPools,
  throughputSeries,
  topologyFromOsds,
} from "./adapters";

const osd = (id: number, host: string | null): Osd => ({
  id,
  cluster_id: "c",
  up: true,
  in_cluster: true,
  host,
});

const pool = (over: Partial<StoragePool>): StoragePool => ({
  id: "p",
  cluster_id: "c",
  name: "p",
  kind: "rbd",
  health: "ok",
  ...over,
});

const sample = (second: number, read: number): MetricHistoryPoint => ({
  ts: new Date(Date.UTC(2026, 0, 1, 0, 0, second)).toISOString(),
  raw_capacity_bytes: 0,
  used_capacity_bytes: 0,
  volumes: 0,
  snapshots: 0,
  read_bytes: read,
  write_bytes: 0,
  read_ops: 0,
  write_ops: 0,
  jobs_running: 0,
  alerts_open: 0,
});

describe("adapters", () => {
  it("groups OSDs by host, with unreported hosts last", () => {
    const nodes = topologyFromOsds([osd(2, "node10"), osd(9, null), osd(1, "node2"), osd(0, "node2")])!;
    expect(nodes.map((n) => n.host)).toEqual(["node2", "node10", null]);
    expect(nodes[0].disks.map((d) => d.label)).toEqual(["osd.0", "osd.1"]);
    expect(topologyFromOsds(undefined)).toBeNull();
  });

  it("collects distinct replica sizes", () => {
    expect(
      replicaSizesFromPools([pool({ replica_size: 3 }), pool({ replica_size: null }), pool({ replica_size: 2 })]),
    ).toEqual([2, 3]);
  });

  it("keeps the two meanings of max_bytes apart", () => {
    const input = [pool({ used_bytes: 10, max_bytes: 100 })];
    expect(poolUsageFromPools(input, "total")![0]).toMatchObject({ capacityBytes: 100, availableBytes: null });
    expect(poolUsageFromPools(input, () => "available")![0]).toMatchObject({
      capacityBytes: null,
      availableBytes: 100,
    });
  });

  it("takes Ceph's own percentage from ceph df", () => {
    const [p] = poolUsageFromCephDf({
      pools: [{ id: 1, name: "rbd", stats: { stored: 5, max_avail: 15, percent_used: 0.25 } }],
    })!;
    expect(p).toMatchObject({ id: "1", usedBytes: 5, availableBytes: 15, percentUsed: 25 });
    expect(poolUsageFromCephDf(undefined)).toBeNull();
    expect(poolUsageFromCephDf({ pools: [{ name: "x" }] })![0].percentUsed).toBeNull();
  });

  it("treats zero raw capacity as not surveyed", () => {
    expect(capacityFromSummary({ raw_capacity_bytes: 0, used_capacity_bytes: 0 } as MetricsSummary)).toEqual({
      usedBytes: null,
      totalBytes: null,
    });
  });

  it("turns counters into rates and a counter reset into a gap", () => {
    const [read] = throughputSeries([sample(20, 50), sample(0, 100), sample(10, 300)])!;
    expect(read.points.map((p) => p.v)).toEqual([20, null]);
    expect(throughputSeries([sample(0, 1)])![0].points).toEqual([]);
  });

  it("leaves idle latency columns empty", () => {
    const [s] = latencySeries([
      {
        device: "sda",
        op: "read",
        columns: [
          { start_unix: 60, count: 0, peak_us: 0 },
          { start_unix: 0, count: 4, peak_us: 800 },
        ],
      },
    ])!;
    expect(s.points).toEqual([
      { t: 0, v: 800 },
      { t: 60000, v: null },
    ]);
  });
});
