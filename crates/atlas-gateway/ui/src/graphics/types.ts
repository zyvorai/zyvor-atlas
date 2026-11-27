// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Input shapes for the graphics. `null`/`undefined` always means "not reported" and is
// rendered as such: the components never substitute a number.
import type { Health } from "../api/types";

export interface HeroDisk {
  /** Stable key, unique across the whole cluster. */
  id: string;
  label: string;
  up: boolean;
  inCluster: boolean;
  deviceClass?: string | null;
  usedBytes?: number | null;
  capacityBytes?: number | null;
}

export interface HeroNode {
  /** `null` groups disks whose host was not reported. */
  host: string | null;
  disks: readonly HeroDisk[];
}

export interface PoolUsage {
  id: string;
  name: string;
  usedBytes?: number | null;
  /** Total size, in the same units as `usedBytes`. */
  capacityBytes?: number | null;
  /** Remaining space. Shown as text only; no fill level is derived from it. */
  availableBytes?: number | null;
  /** Fill level (0-100) reported by the backend itself. Wins over used / capacity. */
  percentUsed?: number | null;
  health?: Health | null;
}

export interface TrendPoint {
  /** Epoch milliseconds. */
  t: number;
  /** `null` is a gap: the line breaks there. */
  v: number | null;
}

export interface TrendSeries {
  id: string;
  label: string;
  points: readonly TrendPoint[];
}
