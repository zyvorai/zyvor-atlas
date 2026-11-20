// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
/** Depth ramp for anything that fills — state colour, never interaction colour. */

export type DepthStop = {
  /** CSS utility class: dp1…dp5 */
  cls: "dp1" | "dp2" | "dp3" | "dp4" | "dp5";
  name: "shoal" | "shelf" | "slope" | "deep" | "abyssal";
  max: number;
};

const STOPS: DepthStop[] = [
  { max: 40, cls: "dp1", name: "shoal" },
  { max: 60, cls: "dp2", name: "shelf" },
  { max: 75, cls: "dp3", name: "slope" },
  { max: 90, cls: "dp4", name: "deep" },
  { max: 101, cls: "dp5", name: "abyssal" },
];

/** Map a 0–100 occupancy percent onto the bathymetric depth ramp. */
export function depth(pct: number | null | undefined): DepthStop {
  const p = Math.max(0, Math.min(100, Number(pct) || 0));
  return STOPS.find((d) => p < d.max) ?? STOPS[STOPS.length - 1];
}

/** Trough fill width — keep a hairline visible even at 0%. */
export function depthWidth(pct: number | null | undefined): string {
  const p = Math.max(0, Math.min(100, Number(pct) || 0));
  return `${Math.max(p, 0.6)}%`;
}
