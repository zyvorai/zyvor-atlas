// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Overview: a health headline, a grid of feature tiles, then pools, activity and clusters.
// Every number and shape here comes from the gateway; anything unknown reads as "—" or says so.
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useNavigate } from "react-router-dom";
import { ChevronRight } from "lucide-react";
import { useAlerts, useCephOsdDf, useClusters, useForecast, useHistory, useOsds, usePools, useSummary } from "../api/hooks";
import type { Health } from "../api/types";
import { history, onHistory, recordSummary, seed } from "../store/history";
import { depth, depthWidth } from "../lib/depth";
import { sendPrompt } from "../lib/prompts";
import { cx, fmtBytes, fmtBytesOpt, fmtForecastFill, fmtPct, fmtSi, healthKind, num } from "../lib/format";
import { Badge } from "../ui/kit";
import { ThroughputChart } from "../ui/ThroughputChart";
import { DashboardHero, DashboardModule } from "../ui/templates/DashboardHero";
import "./Overview.css";

const HEADLINE: Record<Health, string> = {
  ok: "Storage is healthy.",
  warn: "Storage needs attention.",
  critical: "Storage is critical.",
  unknown: "Your storage, at a glance.",
};

/** The worst health any cluster reports; "ok" only when every cluster says so. */
function worstHealth(values: (Health | undefined)[]): Health {
  if (values.includes("critical")) return "critical";
  if (values.includes("warn")) return "warn";
  return values.length && values.every((h) => h === "ok") ? "ok" : "unknown";
}

const dash = (n: number | null | undefined) => (typeof n === "number" ? num(n) : "—");

function CapacityRing({ pct }: { pct: number | null }) {
  const r = 104;
  const circ = 2 * Math.PI * r;
  const tone = pct == null || pct < 75 ? "" : pct < 90 ? "warn" : "fail";
  return (
    <svg
      className={cx("ov-ring", tone)}
      viewBox="0 0 240 240"
      role="img"
      aria-label={pct == null ? "Capacity not surveyed" : `${pct} percent of capacity used`}
    >
      <circle className="track" cx="120" cy="120" r={r} />
      {pct != null && pct > 0 && (
        <circle
          className="value"
          cx="120"
          cy="120"
          r={r}
          transform="rotate(-90 120 120)"
          style={{ strokeDasharray: circ, strokeDashoffset: circ * (1 - pct / 100), ["--ov-circ" as string]: circ }}
        />
      )}
      <text className="figure" x="120" y="128" textAnchor="middle">
        {pct == null ? "—" : `${pct}%`}
      </text>
      <text className="caption" x="120" y="156" textAnchor="middle">
        {pct == null ? "not surveyed" : "used"}
      </text>
    </svg>
  );
}

function Stat({
  label,
  value,
  unit,
  onOpen,
  children,
}: {
  label: string;
  value: ReactNode;
  unit?: string;
  onOpen: () => void;
  children?: ReactNode;
}) {
  return (
    <article className="ov-tile ov-stat a-reveal">
      <h2 className="ov-tile-title">
        <button type="button" className="ov-open" onClick={onOpen}>
          {label}
          <ChevronRight size={16} aria-hidden />
        </button>
      </h2>
      <div className="ov-number">
        {value}
        {unit ? <span className="at-unit">{unit}</span> : null}
      </div>
      <div className="ov-note">{children}</div>
    </article>
  );
}

export default function Overview() {
  const nav = useNavigate();
  const { data: s } = useSummary();
  const { data: clusters } = useClusters();
  const { data: pools } = usePools();
  const { data: osds } = useOsds();
  const { data: osdDf } = useCephOsdDf();
  const { data: alerts } = useAlerts("open");
  const { data: forecast } = useForecast();
  const io = s?.client_io;
  const rc = s?.recovery;
  const recovering = rc ? (rc.pg_recovering || 0) + (rc.pg_backfilling || 0) : null;
  const [, setTick] = useState(0);
  const { data: serverHist } = useHistory();
  useEffect(() => onHistory(() => setTick((t) => t + 1)), []);
  useEffect(() => {
    if (serverHist) seed(serverHist);
  }, [serverHist]);
  useEffect(() => {
    if (s) recordSummary(s);
  }, [s]);
  const hist = history();

  const osdRows = useMemo(() => {
    const byNum: Record<number, { used: number; cap: number }> = {};
    for (const n of osdDf?.nodes || []) {
      if (typeof n?.id !== "number") continue;
      byNum[n.id] = { used: (n.kb_used || 0) * 1024, cap: (n.kb || 0) * 1024 };
    }
    return (osds || []).map((o) => {
      const live = byNum[o.osd_num ?? o.id];
      if (!live) return o;
      return { ...o, used_bytes: o.used_bytes ?? live.used, capacity_bytes: o.capacity_bytes ?? live.cap };
    });
  }, [osds, osdDf]);

  const hasSurvey = (s?.raw_capacity_bytes ?? 0) > 0;
  const usedPct = hasSurvey ? fmtPct(s?.used_capacity_percent) : null;
  const health = worstHealth((clusters || []).map((c) => c.health));
  const hostCount = new Set(osdRows.map((o) => o.host).filter(Boolean)).size;

  const poolRows = useMemo(() => {
    return (pools || [])
      .map((p) => {
        const known = p.used_bytes != null || p.max_bytes != null;
        const used = p.used_bytes || 0;
        const total = (p.max_bytes || 0) + used;
        const pct = total > 0 ? (used / total) * 100 : 0;
        return { ...p, known, used, total, pct };
      })
      .sort((a, b) => b.pct - a.pct);
  }, [pools]);

  const fullest = poolRows[0];
  const forecastLine = fmtForecastFill(forecast?.days_to_full, forecast?.growth_bytes_per_day);

  const summaryLine: ReactNode[] = [];
  if (!hasSurvey) {
    summaryLine.push("Capacity not surveyed yet.");
  } else {
    summaryLine.push(
      <span key="cap">
        <b>{fmtBytesOpt(s?.used_capacity_bytes)}</b> used of {fmtBytesOpt(s?.raw_capacity_bytes)}.
        {typeof s?.volumes === "number" ? ` ${s.volumes} volume${s.volumes === 1 ? "" : "s"}.` : ""}
      </span>,
    );
  }
  if (health === "warn" || health === "critical") {
    const tip = alerts?.[0]?.title;
    summaryLine.push(
      <span key="health">
        {" "}
        Cluster reports <b className={health === "critical" ? "at-state-crit" : "at-state-warn"}>{health}</b>
        {tip ? ` — ${tip}` : ""}.
      </span>,
    );
  } else if (fullest && fullest.pct >= 60) {
    summaryLine.push(
      <span key="pool">
        {" "}
        Pool <b>{fullest.name}</b> is {Math.round(fullest.pct)}% full.
      </span>,
    );
  } else if (recovering) {
    summaryLine.push(
      <span key="rec">
        {" "}
        <b>{recovering}</b> PG{recovering === 1 ? "" : "s"} rebuilding.
      </span>,
    );
  }

  const osdUp = osdRows.filter((o) => o.up).length;
  const osdIn = osdRows.filter((o) => o.in_cluster).length;
  const osdTotal = osdRows.length;

  const ledger: { key: string; t: string; cls: string; body: ReactNode }[] = [];
  for (const a of (alerts || []).slice(0, 4)) {
    const t = a.created_at ? new Date(a.created_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) : "—";
    ledger.push({
      key: a.id,
      t,
      cls: a.severity === "critical" ? "dp5" : a.severity === "warning" ? "dp4" : "dp3",
      body: (
        <>
          <span className="sr-only">{a.severity}: </span>
          {a.title}
          {a.resource_id ? (
            <>
              {" "}
              <code>{a.resource_id}</code>
            </>
          ) : null}
        </>
      ),
    });
  }
  if (fullest && fullest.pct >= 40) {
    ledger.push({
      key: "pool",
      t: "now",
      cls: depth(fullest.pct).cls,
      body: (
        <>
          Pool <code>{fullest.name}</code> at {Math.round(fullest.pct)}% of capacity
        </>
      ),
    });
  }
  if (forecastLine) ledger.push({ key: "forecast", t: "next", cls: "dp2", body: forecastLine });

  const noBuckets = s?.buckets === 0 && s?.backups === 0;

  return (
    <DashboardHero
      hero
      eyebrow={
        <>
          Overview
          {clusters?.length === 1 ? ` · ${clusters[0].name}` : ""}
          {(clusters?.length ?? 0) > 1 ? ` · ${clusters!.length} clusters` : ""}
          {hostCount ? ` · ${hostCount} host${hostCount === 1 ? "" : "s"}` : ""}
        </>
      }
      title={HEADLINE[health]}
      state={summaryLine}
      actions={
        <>
          <button type="button" className="at-btn primary large" onClick={() => nav("/volumes")}>
            New volume
          </button>
          <button type="button" className="a-link ov-more" onClick={() => nav("/observatory")}>
            Open Observatory
            <ChevronRight size={16} aria-hidden />
          </button>
        </>
      }
    >
      <div className="ov-grid">
        <section className="ov-tile ov-capacity" aria-labelledby="ov-capacity-title">
          <h2 id="ov-capacity-title" className="ov-headline">
            Capacity
          </h2>
          <p className="ov-lede">{forecastLine || (hasSurvey ? "No near-term fill projected." : "Waiting on cluster capacity discovery.")}</p>
          <CapacityRing pct={usedPct} />
          <dl className="ov-figures">
            <div>
              <dt>
                <span className="ov-key used" aria-hidden />
                Used
              </dt>
              <dd>{hasSurvey ? fmtBytesOpt(s?.used_capacity_bytes) : "—"}</dd>
            </div>
            <div>
              <dt>
                <span className="ov-key free" aria-hidden />
                Free
              </dt>
              <dd>{hasSurvey ? fmtBytesOpt(s?.available_capacity_bytes) : "—"}</dd>
            </div>
            <div>
              <dt>Raw</dt>
              <dd>{hasSurvey ? fmtBytesOpt(s?.raw_capacity_bytes) : "—"}</dd>
            </div>
          </dl>
        </section>

        <section className="ov-tile ov-io" aria-labelledby="ov-io-title">
          <h2 id="ov-io-title" className="ov-tile-title">
            Throughput
          </h2>
          <ThroughputChart hist={hist} />
          <dl className="ov-figures">
            <div>
              <dt>Read, total</dt>
              <dd>
                {io ? fmtSi(io.read_ops_total) : "—"} <small>ops</small>
              </dd>
              <dd className="sub">{io ? fmtBytes(io.read_bytes_total) : "—"}</dd>
            </div>
            <div>
              <dt>Written, total</dt>
              <dd>
                {io ? fmtSi(io.write_ops_total) : "—"} <small>ops</small>
              </dd>
              <dd className="sub">{io ? fmtBytes(io.write_bytes_total) : "—"}</dd>
            </div>
          </dl>
        </section>

        <section className="ov-tile ov-osds a-reveal" aria-labelledby="ov-osd-title">
          <h2 id="ov-osd-title" className="ov-tile-title">
            <button type="button" className="ov-open" onClick={() => nav("/ceph")}>
              OSDs
              <ChevronRight size={16} aria-hidden />
            </button>
          </h2>
          <div className="ov-number">
            {osdTotal ? osdUp : "—"}
            {osdTotal ? <span className="at-unit">of {osdTotal} up</span> : null}
          </div>
          {osdTotal ? (
            <>
              <ul className="ov-osd-grid" aria-label="OSD states">
                {osdRows.map((o) => {
                  const state = !o.up ? "down" : o.in_cluster ? "up" : "out";
                  const label = `osd.${o.osd_num ?? o.id} · ${state === "out" ? "up, out of cluster" : state}`;
                  return (
                    <li key={o.id} className={state} title={label}>
                      <span className="sr-only">{label}</span>
                    </li>
                  );
                })}
              </ul>
              <div className="ov-note">
                {osdIn} in cluster
                {osdTotal - osdUp > 0 ? ` · ${osdTotal - osdUp} down` : ""}
              </div>
            </>
          ) : (
            <div className="ov-note">No OSDs reported.</div>
          )}
        </section>

        <Stat label="Volumes" value={dash(s?.volumes)} onOpen={() => nav("/volumes")}>
          {s?.volumes === 0 ? (
            <>
              No volumes yet.{" "}
              <button type="button" className="a-link" onClick={() => nav("/volumes")}>
                Create a volume →
              </button>
            </>
          ) : (
            "Block and filesystem"
          )}
        </Stat>
        <Stat label="Snapshots" value={dash(s?.snapshots)} onOpen={() => nav("/snapshots")}>
          {s?.snapshots === 0 ? (
            <>
              No point-in-time copies yet.{" "}
              <button
                type="button"
                className="a-link"
                onClick={() =>
                  sendPrompt("Create a nightly snapshot schedule with 14-day retention for the busiest pools.")
                }
              >
                Schedule nightly snapshots →
              </button>
            </>
          ) : (
            "Protection points"
          )}
        </Stat>
        <Stat
          label="Buckets and backups"
          value={
            <>
              {dash(s?.buckets)}
              <span className="ov-slash">/</span>
              {dash(s?.backups)}
            </>
          }
          onOpen={() => nav("/buckets")}
        >
          {noBuckets ? (
            <>
              Object storage unused.{" "}
              <button type="button" className="a-link" onClick={() => nav("/buckets")}>
                Create first bucket →
              </button>
            </>
          ) : (
            "Object storage"
          )}
        </Stat>
        <Stat label="Recovery" value={dash(recovering)} unit="PG rebuilding" onOpen={() => nav("/ceph")}>
          {rc ? `${num(rc.objects_degraded)} degraded · ${num(rc.objects_unfound)} unfound` : "Not reported"}
        </Stat>
      </div>

      <DashboardModule title="Pools" note={`${poolRows.length} pool${poolRows.length === 1 ? "" : "s"}, fullest first`} className="a-reveal">
        <div className="at-panel">
          {poolRows.length === 0 && <div className="at-muted-row">No pools discovered yet.</div>}
          {poolRows.map((p) => {
            const d = depth(p.pct);
            return (
              <button key={p.id} type="button" className="at-basin" onClick={() => nav(`/pools/${p.id}`)}>
                <div className="at-basin-id">
                  <div className="at-basin-name">
                    {p.name}
                    <span className="at-tag">{p.kind || "pool"}</span>
                  </div>
                  <div className="at-trough">
                    {p.known && <span className={`at-level ${d.cls}`} style={{ width: depthWidth(p.pct) }} />}
                  </div>
                </div>
                <div className="at-basin-read">
                  {p.known ? fmtBytes(p.used) : "—"}
                  <small>of {p.known ? fmtBytes(p.total) : "—"}</small>
                </div>
                <div className={`at-basin-pct ${d.cls}`}>{p.known ? `${Math.round(p.pct)}%` : "—"}</div>
              </button>
            );
          })}
        </div>
      </DashboardModule>

      <div className="at-2col">
        <DashboardModule title="Activity" note="Alerts and forecasts" className="a-reveal">
          <div className="at-panel at-ledger">
            {ledger.length === 0 && <div className="at-muted-row">No recent exceptions.</div>}
            {ledger.map((e) => (
              <div key={e.key} className="at-entry">
                <time>{e.t}</time>
                <span className={`mk ${e.cls}`} aria-hidden />
                <span className="txt">{e.body}</span>
              </div>
            ))}
          </div>
        </DashboardModule>

        <DashboardModule title="Clusters" note={`${clusters?.length || 0} registered`} className="a-reveal">
          <div className="at-panel">
            {clusters?.length ? (
              <div className="overflow-x-auto">
                <table className="at-tbl">
                  <thead>
                    <tr>
                      <th>Name</th>
                      <th>Health</th>
                      <th>Raw</th>
                      <th>Used</th>
                    </tr>
                  </thead>
                  <tbody>
                    {clusters.map((c) => (
                      <tr key={c.id} className="cursor-pointer" onClick={() => nav("/cluster")}>
                        <td>
                          <button type="button" className="a-link">
                            {c.name}
                          </button>
                        </td>
                        <td>
                          <Badge kind={healthKind(c.health)} dot>
                            {c.health}
                          </Badge>
                        </td>
                        <td className="mono">{fmtBytesOpt(c.raw_capacity_bytes)}</td>
                        <td className="mono">{fmtBytesOpt(c.used_capacity_bytes)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            ) : (
              <div className="at-muted-row">No clusters.</div>
            )}
          </div>
        </DashboardModule>
      </div>
    </DashboardHero>
  );
}
