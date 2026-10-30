// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
// Fast storage: NVMe drive health, host CPU/RAM sizing, OSD object store mix, and the atlas-io
// agent's heuristic I/O shapes. Read-only; every number comes from the gateway, nothing is inferred
// here, and an unconfigured or unreachable source says so instead of looking healthy.
import { useCephNvme, useCephObjectStore, useCephSizing, useIoAi } from "../api/hooks";
import type { IoAiDevice, IoShapeName, NvmeDrive, SizingHost } from "../api/types";
import { Badge } from "../ui/kit";
import { DashboardHero } from "../ui/templates/DashboardHero";
import { navCrumbs } from "../nav/routes";
import { Table } from "../ui/Table";
import { fmtBytes } from "../lib/format";

const SHAPE_LABEL: Record<IoShapeName, string> = {
  large_write: "large writes (checkpoint-like)",
  bulk_read: "bulk reads (dataset / model load)",
  small_read: "small reads (latency-sensitive)",
  mixed: "mixed",
  unknown: "too few requests to tell",
};

const dash = (v?: number | null, unit = "") => (v === null || v === undefined ? "—" : `${v}${unit}`);

function micros(us: number): string {
  if (!us) return "—";
  return us >= 1000 ? `${(us / 1000).toFixed(1)} ms` : `${us} µs`;
}

export default function FastStorage() {
  const nvme = useCephNvme();
  const sizing = useCephSizing();
  const stores = useCephObjectStore();
  const ioai = useIoAi();

  const drives = nvme.data?.devices ?? [];
  const hosts = sizing.data?.hosts ?? [];
  const ioDevices = ioai.data?.devices ?? [];
  const concerns = nvme.data?.summary.with_concerns ?? 0;
  const undersized = sizing.data?.summary.undersized ?? 0;
  const mixed = !!stores.data?.summary.mixed_versions;
  const slowShapes = ioDevices.filter((d) => d.finding).length;

  return (
    <DashboardHero
      className="at-stack"
      crumbs={navCrumbs("fast-storage")}
      eyebrow="INFRASTRUCTURE · NVME"
      title="Fast storage"
      state={
        nvme.isError && sizing.isError && ioai.data?.status !== "ok"
          ? "No NVMe, sizing or I/O data yet — a Ceph backend and (optionally) the atlas-io agent are needed."
          : `${drives.length} NVMe drive${drives.length === 1 ? "" : "s"} · ${concerns} with concerns · ${undersized} undersized host${undersized === 1 ? "" : "s"}.`
      }
    >
      <div className="at-instrs">
        <div className="at-instr">
          <div className="at-caption">NVMe drives</div>
          <div className="at-val md">{nvme.data ? drives.length : "—"}</div>
          <div className="at-delta mono">{concerns} with concerns</div>
        </div>
        <div className="at-instr">
          <div className="at-caption">Undersized hosts</div>
          <div className="at-val md">{sizing.data ? undersized : "—"}</div>
          <div className="at-delta">rule of thumb, not a requirement</div>
        </div>
        <div className="at-instr">
          <div className="at-caption">Ceph versions</div>
          <div className="at-val md">{stores.data ? Object.keys(stores.data.summary.by_version).length : "—"}</div>
          <div className="at-delta">{mixed ? "mixed (upgrade in progress?)" : "uniform"}</div>
        </div>
        <div className="at-instr">
          <div className="at-caption">I/O shapes</div>
          <div className="at-val md" style={{ textTransform: "uppercase" }}>
            {ioai.data ? (ioai.data.status === "ok" ? `${ioDevices.length} dev` : "unavailable") : "—"}
          </div>
          <div className="at-delta">{slowShapes} missing latency target</div>
        </div>
      </div>

      <Table<NvmeDrive>
        soundings
        panelTitle="NVMe drive health"
        rows={nvme.isError ? undefined : drives}
        error={nvme.isError}
        onRetry={() => void nvme.refetch()}
        rowKey={(d) => d.devid}
        empty="No NVMe drives reported by Ceph."
        cols={[
          { h: "Drive", f: (d) => `${d.host}:${d.dev}`, mono: true, sortKey: (d) => `${d.host}:${d.dev}` },
          { h: "Wear", f: (d) => (d.scraped ? dash(d.wear_percent, "%") : "—"), sortKey: (d) => d.wear_percent ?? -1 },
          { h: "Spare", f: (d) => (d.scraped ? dash(d.available_spare_percent, "%") : "—") },
          { h: "Media errors", f: (d) => (d.scraped ? dash(d.media_errors) : "—") },
          { h: "Temp", f: (d) => (d.scraped ? dash(d.temperature_c, " °C") : "—") },
          {
            h: "Status",
            f: (d) =>
              !d.scraped ? (
                <Badge kind="neutral" title={d.error}>no SMART data</Badge>
              ) : d.concerns.length ? (
                <Badge kind="warning" dot title={d.concerns.join("; ")}>{d.concerns[0]}</Badge>
              ) : (
                <Badge kind="success" dot>healthy</Badge>
              ),
          },
        ]}
      />

      <Table<SizingHost>
        soundings
        panelTitle="Host CPU and RAM vs NVMe rule of thumb"
        panelExtra={
          sizing.data ? (
            <span className="at-sub mono" style={{ margin: 0 }}>
              {sizing.data.thresholds.cores_per_nvme_device} cores/NVMe · {sizing.data.thresholds.ram_base_gib} GiB +{" "}
              {sizing.data.thresholds.ram_per_osd_gib} GiB/OSD — heuristic
            </span>
          ) : undefined
        }
        rows={sizing.isError ? undefined : hosts}
        error={sizing.isError}
        onRetry={() => void sizing.refetch()}
        rowKey={(h) => h.host}
        empty="No hosts with NVMe drives."
        cols={[
          { h: "Host", f: (h) => h.host, mono: true },
          { h: "NVMe", f: (h) => h.nvme_devices },
          { h: "OSDs", f: (h) => h.osds },
          { h: "CPU (has / wants)", f: (h) => (h.has_cpu_cores === undefined ? `— / ${h.needs_cpu_cores}` : `${h.has_cpu_cores} / ${h.needs_cpu_cores}`) },
          { h: "RAM GiB (has / wants)", f: (h) => (h.has_memory_gib === undefined ? `— / ${h.needs_memory_gib}` : `${h.has_memory_gib.toFixed(1)} / ${h.needs_memory_gib}`) },
          {
            h: "Verdict",
            f: (h) =>
              h.status === "ok" ? (
                <Badge kind="success" dot>ok</Badge>
              ) : h.status === "undersized" ? (
                <Badge kind="warning" dot title={(h.shortfalls ?? []).join("; ")}>undersized</Badge>
              ) : (
                <Badge kind="neutral" title="No Kubernetes node matched this Ceph host, so nothing is judged.">unknown</Badge>
              ),
          },
        ]}
      />

      <div className="at-panel">
        <div className="at-panel-bar">
          <span className="at-caption">Storage I/O shapes (atlas-io agent)</span>
          <span className="grow" />
          {ioai.data && (
            <Badge kind={ioai.data.status === "ok" ? "success" : "neutral"} dot>
              {ioai.data.status === "ok" ? "agent reachable" : "unavailable"}
            </Badge>
          )}
        </div>
        {ioai.data?.status === "unavailable" && (
          <div className="at-list-row">
            <span className="at-sub" style={{ margin: 0 }}>
              Not assessed: {ioai.data.detail ?? "the atlas-io agent could not be reached"}. This is not a healthy reading.
            </span>
          </div>
        )}
        {ioai.data?.status === "ok" && ioDevices.length === 0 && (
          <div className="at-list-row">
            <span className="at-sub" style={{ margin: 0 }}>The agent reported no devices yet.</span>
          </div>
        )}
        {ioDevices.map((d: IoAiDevice) => (
          <div key={d.device} className="at-list-row" style={{ alignItems: "center" }}>
            <span className="mono" style={{ fontSize: 12.5, color: "var(--at-ink)", minWidth: 110 }}>{d.device}</span>
            <Badge kind={d.finding ? "warning" : d.shape === "unknown" ? "neutral" : "info"} dot>
              {SHAPE_LABEL[d.shape] ?? d.shape}
            </Badge>
            <span className="at-sub mono" style={{ margin: 0, flex: 1 }}>
              reads avg {fmtBytes(d.avg_read_bytes)} p99 {micros(d.p99_read_us)} · writes avg {fmtBytes(d.avg_write_bytes)} p99 {micros(d.p99_write_us)}
            </span>
            {d.finding && <span style={{ color: "var(--at-warn)", fontSize: 12.5 }}>{d.finding}</span>}
          </div>
        ))}
        {!ioai.data && (
          <div className="at-list-row">
            <span className="at-sub" style={{ margin: 0 }}>Loading…</span>
          </div>
        )}
        <div className="at-list-row">
          <span className="at-sub" style={{ margin: 0 }}>
            A heuristic over block-layer sizes and latency. It names a shape, never a job, and cannot see file names or which
            framework issued the I/O.
          </span>
        </div>
      </div>

      <div className="at-panel">
        <div className="at-panel-bar">
          <span className="at-caption">OSD object store and Ceph version</span>
          <span className="grow" />
          {stores.data && mixed && <Badge kind="warning" dot>mixed versions</Badge>}
        </div>
        {stores.isError && (
          <div className="at-list-row">
            <span className="at-sub" style={{ margin: 0 }}>Could not read OSD metadata from Ceph.</span>
          </div>
        )}
        {stores.data && (
          <>
            <div className="at-list-row">
              <span className="at-sub" style={{ margin: 0 }}>
                Object store:{" "}
                {Object.entries(stores.data.summary.by_objectstore)
                  .map(([k, n]) => `${k} × ${n}`)
                  .join(", ") || "—"}
              </span>
            </div>
            <div className="at-list-row">
              <span className="at-sub" style={{ margin: 0 }}>
                Version:{" "}
                {Object.entries(stores.data.summary.by_version)
                  .map(([k, n]) => `${k} × ${n}`)
                  .join(", ") || "—"}
              </span>
            </div>
          </>
        )}
      </div>
    </DashboardHero>
  );
}
