// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
/**
 * Pool figures for the graphics. `StoragePool.max_bytes` means different things per driver:
 * the Ceph drivers put the space still available there (and the real one counts replicas in
 * `used_bytes`), while ZFS, NFS, WEKA, macOS and the other drivers put the pool's total size.
 * So `used_bytes / max_bytes` is only a fill level for the second group; for Ceph the level is the
 * percentage Ceph itself reports in `/ceph/df`.
 */
import type { StorageCluster, StoragePool } from "../api/types";
import { poolUsageFromCephDf, poolUsageFromPools } from "../graphics/adapters";
import type { PoolUsage } from "../graphics/types";

/** The backend `/ceph/df` answers for (`CEPH_BACKEND_ID` in the gateway). */
const CEPH_DF_BACKEND = "bkd_ceph_lab";
const CEPH_KINDS = new Set(["rbd", "cephfs_data", "cephfs_metadata", "rgw"]);
const TOTAL_KINDS = new Set(["nfs_export", "zpool"]);

type BackendType = { backend_id: string; backend_type: string };

export function poolUsage(
  pools: readonly StoragePool[] | null | undefined,
  {
    clusters,
    backends,
    cephDf,
  }: {
    clusters?: readonly StorageCluster[] | null;
    backends?: readonly BackendType[] | null;
    cephDf?: unknown;
  },
): PoolUsage[] | null {
  if (!pools) return null;
  const backendOfCluster = new Map((clusters ?? []).map((c) => [c.id, c.backend_id]));
  const typeOfBackend = new Map((backends ?? []).map((b) => [b.backend_id, b.backend_type]));
  const fromDf = new Map((poolUsageFromCephDf(cephDf) ?? []).map((p) => [p.name, p]));

  return pools.map((pool) => {
    const backend = backendOfCluster.get(pool.cluster_id);
    const type = backend === undefined ? undefined : typeOfBackend.get(backend);
    const ceph = type ? type === "ceph" : backend === CEPH_DF_BACKEND || CEPH_KINDS.has(pool.kind);

    if (ceph) {
      const df = backend === CEPH_DF_BACKEND ? fromDf.get(pool.name) : undefined;
      if (df) return { ...df, id: pool.id, health: pool.health };
      return poolUsageFromPools([pool], "available")![0];
    }
    if (type || TOTAL_KINDS.has(pool.kind)) return poolUsageFromPools([pool], "total")![0];
    // The driver is unknown, so what max_bytes means is too: show usage only.
    return { id: pool.id, name: pool.name, health: pool.health, usedBytes: pool.used_bytes ?? null };
  });
}

/** Fill level in percent, or `null` when the inputs cannot say. */
export function poolFill(p: PoolUsage): number | null {
  if (typeof p.percentUsed === "number") return p.percentUsed;
  if (typeof p.usedBytes === "number" && typeof p.capacityBytes === "number" && p.capacityBytes > 0) {
    return (p.usedBytes / p.capacityBytes) * 100;
  }
  return null;
}
