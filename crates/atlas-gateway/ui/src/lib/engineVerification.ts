// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
/** Honest live-verification matrix for DataBridge engines (mirrors docs/DATABRIDGE.md). */

export type EngineLiveStage =
  | "discover"
  | "full-load"
  | "validate"
  | "cdc"
  | "cutover";

export type EngineVerification = {
  engine: string;
  /** Stages verified live on real infra (not fake). */
  live: EngineLiveStage[];
  note: string;
};

export const ENGINE_VERIFICATION: EngineVerification[] = [
  {
    engine: "postgres",
    live: ["discover", "full-load", "validate", "cdc", "cutover"],
    note: "End-to-end incl. Debezium CDC on Rook Ceph.",
  },
  {
    engine: "mysql",
    live: ["discover", "full-load", "validate", "cdc", "cutover"],
    note: "End-to-end incl. TIMESTAMP columns (Debezium JDBC sink) → PXC on Rook Ceph.",
  },
  {
    engine: "mariadb",
    live: ["discover", "full-load", "validate", "cdc", "cutover"],
    note: "End-to-end incl. Debezium MariaDB CDC → PXC on Rook Ceph (utf8mb4_unicode_ci; avoid UCA 14 collations).",
  },
  {
    engine: "mongodb",
    live: ["discover", "full-load", "validate", "cdc", "cutover"],
    note: "End-to-end incl. Debezium Mongo CDC → PSMDB (snapshot.mode=no_data; sink topics.regex post-RegexRouter).",
  },
  {
    engine: "sqlserver",
    live: ["discover", "full-load", "cdc", "cutover"],
    note: "Heterogeneous → CloudNativePG; the Debezium initial snapshot is the full-load. Validate is advisory.",
  },
  {
    engine: "oracle",
    live: ["discover", "full-load", "cdc", "cutover"],
    note: "Heterogeneous (26ai, CDB/PDB) → CloudNativePG; the Debezium initial snapshot is the full-load. Validate is advisory.",
  },
];

export function verificationFor(kind: string): EngineVerification | undefined {
  const k = kind.toLowerCase();
  return ENGINE_VERIFICATION.find((e) => e.engine === k);
}

export function stageBadge(
  v: EngineVerification | undefined,
  stage: EngineLiveStage,
): "live" | "pending" {
  return v?.live.includes(stage) ? "live" : "pending";
}
