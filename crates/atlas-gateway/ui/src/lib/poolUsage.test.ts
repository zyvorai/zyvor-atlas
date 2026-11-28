// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { describe, expect, it } from "vitest";
import type { StorageCluster, StoragePool } from "../api/types";
import { poolFill, poolUsage } from "./poolUsage";

const cluster = (id: string, backend_id: string): StorageCluster => ({ id, backend_id, name: id, health: "ok" });
const pool = (name: string, cluster_id: string, kind: string, used: number, max: number): StoragePool => ({
  id: `pool_${name}`,
  cluster_id,
  name,
  kind,
  used_bytes: used,
  max_bytes: max,
  health: "ok",
});

const clusters = [cluster("ceph", "bkd_ceph_lab"), cluster("tank", "bkd_zfs"), cluster("ext", "bkd_ceph_ext")];
const backends = [
  { backend_id: "bkd_ceph_lab", backend_type: "ceph" },
  { backend_id: "bkd_zfs", backend_type: "zfs" },
  { backend_id: "bkd_ceph_ext", backend_type: "ceph" },
];

describe("poolUsage", () => {
  it("takes a Ceph fill level from ceph df, never from used / max", () => {
    const [p] = poolUsage([pool("rbd", "ceph", "rbd", 300, 100)], {
      clusters,
      backends,
      cephDf: { pools: [{ id: 1, name: "rbd", stats: { stored: 100, max_avail: 100, percent_used: 0.5 } }] },
    })!;
    expect(p.id).toBe("pool_rbd");
    expect(poolFill(p)).toBe(50);
  });

  it("draws no Ceph fill level without ceph df, or for a Ceph backend it does not cover", () => {
    const df = { pools: [{ id: 1, name: "rbd", stats: { stored: 100, max_avail: 100, percent_used: 0.5 } }] };
    const [noDf] = poolUsage([pool("rbd", "ceph", "rbd", 300, 100)], { clusters, backends })!;
    const [external] = poolUsage([pool("rbd", "ext", "rbd", 300, 100)], { clusters, backends, cephDf: df })!;
    for (const p of [noDf, external]) {
      expect(poolFill(p)).toBeNull();
      expect(p.availableBytes).toBe(100);
    }
  });

  it("reads max_bytes as the total for the other drivers", () => {
    const [p] = poolUsage([pool("tank", "tank", "zpool", 25, 100)], { clusters, backends })!;
    expect(poolFill(p)).toBe(25);
  });

  it("claims nothing about max_bytes when the driver is unknown", () => {
    const [p] = poolUsage([pool("x", "nowhere", "other", 25, 100)], {})!;
    expect(poolFill(p)).toBeNull();
    expect(p.availableBytes).toBeUndefined();
    expect(p.capacityBytes).toBeUndefined();
  });
});
