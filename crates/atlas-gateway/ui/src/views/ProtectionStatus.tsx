// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Protection Status: per-volume synthesis of replication, last snapshot/backup, DR mirror state,
// and RPO target-vs-actual into one verdict — GET /protection-status, GET /volumes/{id}/protection.
import type { ClusterHealthState, VolumeProtectionStatus } from "../api/types";
import { useProtectionStatus } from "../api/hooks";
import { apiError } from "../api/client";
import { Badge } from "../ui/kit";
import { ListPage } from "../ui/templates/ListPage";
import { navCrumbs } from "../nav/routes";
import { Table } from "../ui/Table";
import { EmptyState, HealthIndicator, healthMeta } from "../graphics";
import { timeAgo } from "../lib/format";
import "./ProtectionStatus.css";

const WORST_FIRST: ClusterHealthState[] = ["critical", "at_risk", "rebuilding", "degraded", "healthy"];
const word = (v: ClusterHealthState) => healthMeta(v).word;

function fmtSecs(n?: number | null): string {
  if (n == null) return "—";
  if (n < 60) return `${n}s`;
  if (n < 3600) return `${Math.round(n / 60)}m`;
  if (n < 86400) return `${Math.round(n / 3600)}h`;
  return `${Math.round(n / 86400)}d`;
}

export default function ProtectionStatus() {
  const { data: rows, isError, error, refetch } = useProtectionStatus();
  const all = rows || [];

  const counts: Record<ClusterHealthState, number> = {
    healthy: 0,
    degraded: 0,
    rebuilding: 0,
    at_risk: 0,
    critical: 0,
  };
  all.forEach((r) => counts[r.verdict]++);
  const worst = WORST_FIRST.find((v) => counts[v] > 0);
  const volumes = `${all.length} volume${all.length === 1 ? "" : "s"}`;

  const stateLine = !rows
    ? isError
      ? "Couldn't load protection status."
      : "Loading protection status…"
    : !all.length
      ? "No volumes to protect yet."
      : !worst || worst === "healthy"
        ? `${volumes}, all healthy.`
        : `${volumes}. Worst: ${counts[worst]} ${word(worst).toLowerCase()}.`;

  return (
    <ListPage className="at-stack" crumbs={navCrumbs("protection")} eyebrow={null} title="Protection Status" state={stateLine}>
      {rows && !rows.length ? (
        <div className="at-panel">
          <EmptyState
            art="backup"
            title="No volumes to protect yet"
            description="Each volume gets a verdict here once it is in the inventory."
          />
        </div>
      ) : (
        <>
          <section className="at-panel ps-summary" aria-label="Verdict summary">
            {all.length > 0 && (
              // The counts below say the same in words.
              <div className="ps-bar" aria-hidden>
                {WORST_FIRST.filter((v) => counts[v] > 0).map((v) => (
                  <span key={v} className={`ps-seg ps-${v}`} data-verdict={v} style={{ flexGrow: counts[v] }} />
                ))}
              </div>
            )}
            <ul className="ps-counts">
              {WORST_FIRST.map((v) => (
                <li key={v}>
                  <span className="ps-count">{rows ? counts[v] : "—"}</span>
                  <HealthIndicator health={v} />
                </li>
              ))}
            </ul>
          </section>

          <div className="ps-table">
            <Table
              soundings
              panelTitle="Volumes"
              rows={rows}
              error={isError}
              errorDetail={isError ? apiError(error) : undefined}
              onRetry={() => refetch()}
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
                    <span className="ps-verdict" title={r.verdict_reasons.join("; ")}>
                      <HealthIndicator health={r.verdict} />
                    </span>
                  ),
                },
              ]}
            />
          </div>
        </>
      )}
    </ListPage>
  );
}
