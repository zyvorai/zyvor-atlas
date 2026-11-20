// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Protection Status: per-volume synthesis of replication, last snapshot/backup, DR mirror state,
// and RPO target-vs-actual into one verdict — GET /protection-status, GET /volumes/{id}/protection.
import type { CSSProperties } from "react";
import type { ClusterHealthState, VolumeProtectionStatus } from "../api/types";
import { useProtectionStatus } from "../api/hooks";
import { Badge } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { Table } from "../ui/Table";
import { timeAgo } from "../lib/format";

const VERDICT_LABEL: Record<ClusterHealthState, string> = {
  healthy: "Healthy",
  degraded: "Degraded",
  rebuilding: "Rebuilding",
  at_risk: "At Risk",
  critical: "Critical",
};
const verdictKind = (v: ClusterHealthState) =>
  v === "healthy"
    ? "success"
    : v === "degraded"
      ? "warning"
      : v === "rebuilding"
        ? "info"
        : v === "at_risk"
          ? "at-risk"
          : "danger";

function fmtSecs(n?: number | null): string {
  if (n == null) return "—";
  if (n < 60) return `${n}s`;
  if (n < 3600) return `${Math.round(n / 60)}m`;
  if (n < 86400) return `${Math.round(n / 3600)}h`;
  return `${Math.round(n / 86400)}d`;
}

export default function ProtectionStatus() {
  const { data: rows } = useProtectionStatus();
  const all = rows || [];

  const counts: Record<ClusterHealthState, number> = {
    healthy: 0,
    degraded: 0,
    rebuilding: 0,
    at_risk: 0,
    critical: 0,
  };
  all.forEach((r) => counts[r.verdict]++);
  const worstFirst = ["critical", "at_risk", "rebuilding", "degraded", "healthy"] as ClusterHealthState[];
  const headline = worstFirst.find((v) => counts[v] > 0);

  return (
    <ListPage
      className="at-stack"
      crumbs={navCrumbs("protection")}
      eyebrow="DATA PROTECTION"
      title="Protection Status"
      state={
        all.length
          ? `${all.length} volume${all.length === 1 ? "" : "s"} — ${headline ? `worst: ${counts[headline]} ${VERDICT_LABEL[headline].toLowerCase()}` : "all healthy"}.`
          : "No volumes to protect yet."
      }
    >

      <div className="at-instrs" style={{ "--instr-cols": 5 } as CSSProperties}>
        {worstFirst.map((v) => (
          <div key={v} className="at-instr">
            <div className="at-caption">{VERDICT_LABEL[v]}</div>
            <div className="at-val md">
              <Badge kind={verdictKind(v)} dot>
                {counts[v]}
              </Badge>
            </div>
          </div>
        ))}
      </div>

      <Table
        soundings
        panelTitle={
          <>
            Volumes <Badge kind="neutral">{all.length}</Badge>
          </>
        }
        rows={all}
        rowKey={(r: VolumeProtectionStatus) => r.volume_id}
        cols={[
          { h: "Volume", f: (r: VolumeProtectionStatus) => r.volume_name || r.volume_id, mono: true },
          { h: "Backend", f: (r: VolumeProtectionStatus) => r.storage_backend },
          { h: "Replication", f: (r: VolumeProtectionStatus) => (r.replication_factor != null ? `${r.replication_factor}x` : "—") },
          { h: "Last snapshot", f: (r: VolumeProtectionStatus) => timeAgo(r.last_snapshot_at) },
          { h: "Last backup", f: (r: VolumeProtectionStatus) => (r.last_backup_at ? `${timeAgo(r.last_backup_at)}${r.last_backup_location ? ` · ${r.last_backup_location}` : ""}` : "—") },
          {
            h: "DR",
            f: (r: VolumeProtectionStatus) =>
              r.dr_role ? (
                <Badge kind={r.dr_role === "primary" ? "success" : "info"} dot>
                  {r.dr_role}/{r.dr_state}
                </Badge>
              ) : (
                "—"
              ),
          },
          {
            h: "RPO (target / current)",
            f: (r: VolumeProtectionStatus) => `${fmtSecs(r.rpo_target_seconds)} / ${fmtSecs(r.rpo_current_seconds)}`,
          },
          {
            h: "Verdict",
            f: (r: VolumeProtectionStatus) => (
              <Badge kind={verdictKind(r.verdict)} dot title={r.verdict_reasons.join("; ")}>
                {VERDICT_LABEL[r.verdict]}
              </Badge>
            ),
          },
        ]}
      />
    </ListPage>
  );
}
