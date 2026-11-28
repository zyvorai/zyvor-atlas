// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
// Hero illustration of a storage cluster: one chassis per host, one bay per disk, and a
// replication bus when the pools report more than one copy. Everything drawn comes from props.
import { DataPlaceholder } from "./EmptyState";
import { formatBytes, percentOf, plural } from "./format";
import { HealthIndicator, StatusGlyph, type HealthValue, type Tone } from "./HealthIndicator";
import { useSvgId } from "./SpotIllustration";
import type { HeroDisk, HeroNode } from "./types";
import { useElementWidth } from "./useElementWidth";
import "./graphics.css";

export type DiskState = "up" | "out" | "down";

export function diskState(disk: Pick<HeroDisk, "up" | "inCluster">): DiskState {
  if (!disk.up) return "down";
  return disk.inCluster ? "up" : "out";
}

const STATE_META: Record<DiskState, { tone: Tone; word: string; rank: number }> = {
  down: { tone: "fail", word: "down", rank: 0 },
  out: { tone: "warn", word: "up but out of the cluster", rank: 1 },
  up: { tone: "ok", word: "up and in", rank: 2 },
};

const PAD = 12;
const GAP = 16;
const NODE_MIN = 156;
const NODE_MAX = 240;
const ZOOM_MAX = 1.4;
const BAY_W = 18;
const BAY_H = 44;
const BAY_GAP = 6;
const BAYS_PER_ROW = 6;
const HEAD_H = 36;
const FOOT_H = 30;
const BUS_H = 28;
const ROW_GAP = 18;

const worstRank = (node: HeroNode) =>
  node.disks.reduce((worst, d) => Math.min(worst, STATE_META[diskState(d)].rank), 2);

function countStates(disks: readonly HeroDisk[]): Record<DiskState, number> {
  const n = { up: 0, out: 0, down: 0 };
  for (const d of disks) n[diskState(d)]++;
  return n;
}

function problemText(n: Record<DiskState, number>): string {
  return [n.down ? `${n.down} down` : "", n.out ? `${n.out} out` : ""].filter(Boolean).join(", ");
}

export function replicationText(sizes?: readonly number[] | null): string {
  if (!sizes || sizes.length === 0) return "Replication not reported";
  const min = Math.min(...sizes);
  const max = Math.max(...sizes);
  if (min !== max) return `Replica size ${min}–${max} across pools`;
  return max === 1 ? "Replica size 1: no redundant copies" : `Replica size ${max}`;
}

function diskDescription(d: HeroDisk): string {
  const parts = [d.label, STATE_META[diskState(d)].word];
  if (d.deviceClass) parts.push(d.deviceClass);
  const pct = percentOf(d.usedBytes, d.capacityBytes);
  if (pct !== null) {
    parts.push(`${formatBytes(d.usedBytes as number)} of ${formatBytes(d.capacityBytes as number)} used`);
  } else {
    parts.push("usage not reported");
  }
  return parts.join(", ");
}

function hostName(node: HeroNode): string {
  return node.host ?? "Host not reported";
}

function clip(text: string, chars: number): string {
  return text.length > chars ? `${text.slice(0, Math.max(1, chars - 1))}…` : text;
}

/**
 * A host name that fits `chars`: the domain goes first, then the middle, because fleets tend to
 * differ at the end (`…-01`, `…-02`). The full name stays in the tooltip and the text list.
 */
export function shortHost(name: string, chars: number): string {
  if (name.length <= chars) return name;
  const short = /^[\d.]+$/.test(name) || name.includes(" ") ? name : name.split(".")[0];
  if (short.length <= chars) return short;
  const keep = Math.max(2, chars - 1);
  return `${short.slice(0, Math.ceil(keep / 2))}…${short.slice(-Math.floor(keep / 2))}`;
}

function Bay({ disk, x, y }: { disk: HeroDisk; x: number; y: number }) {
  const state = diskState(disk);
  const pct = percentOf(disk.usedBytes, disk.capacityBytes);
  const fill = pct === null ? 0 : (Math.min(100, pct) / 100) * (BAY_H - 16);
  return (
    <g className={`ag-bay ag-bay-${state}`} data-state={state} transform={`translate(${x} ${y})`}>
      <title>{diskDescription(disk)}</title>
      <rect className="ag-bay-slot" width={BAY_W} height={BAY_H} rx="5" />
      {state !== "down" && fill > 0 && (
        <rect className="ag-bay-fill" x="3" y={BAY_H - 14 - fill} width={BAY_W - 6} height={fill} rx="2" />
      )}
      <StatusGlyph tone={STATE_META[state].tone} size={12} x={(BAY_W - 12) / 2} y={BAY_H - 13} />
    </g>
  );
}

export interface ClusterHeroProps {
  /** Hosts and their disks (see `topologyFromOsds`). Missing or empty renders an empty state. */
  nodes?: readonly HeroNode[] | null;
  clusterName?: string;
  health?: HealthValue | null;
  /** Distinct pool replica sizes (see `replicaSizesFromPools`). */
  replicaSizes?: readonly number[] | null;
  /** True only while the cluster reports recovery or backfill; animates the replication bus. */
  rebuilding?: boolean;
  loading?: boolean;
  /** Hosts drawn before the rest fold into one tile. Default 12. */
  maxNodes?: number;
  /** Disks drawn per host before the rest are counted in text. Default 12. */
  maxDisksPerNode?: number;
  className?: string;
}

export function ClusterHero({
  nodes,
  clusterName,
  health,
  replicaSizes,
  rebuilding = false,
  loading,
  maxNodes = 12,
  maxDisksPerNode = 12,
  className = "",
}: ClusterHeroProps) {
  const [ref, width] = useElementWidth(960);
  const chassis = useSvgId("ag-chassis");

  if (!nodes || nodes.length === 0) {
    return (
      <DataPlaceholder
        loading={loading}
        art="cluster"
        title="No disks reported"
        description="The cluster topology appears here once discovery returns disks."
        className={className}
      />
    );
  }

  // When something has to be hidden, trouble is drawn first so truncation never hides it.
  const nodeLimit = Math.max(1, maxNodes);
  const diskLimit = Math.max(1, maxDisksPerNode);
  const ordered = nodes.length > nodeLimit ? [...nodes].sort((a, b) => worstRank(a) - worstRank(b)) : nodes;
  const shown = ordered.slice(0, nodeLimit);
  const hidden = ordered.slice(nodeLimit);
  const allDisks = nodes.flatMap((n) => n.disks);
  const totals = countStates(allDisks);

  const tiles = shown.length + (hidden.length ? 1 : 0);
  // A small cluster on a wide screen is scaled up, so the illustration fills its hero.
  const zoom = Math.max(1, Math.min(ZOOM_MAX, (width - 2 * PAD) / (tiles * (NODE_MAX + GAP))));
  const W = Math.max(width / zoom, NODE_MIN + 2 * PAD);
  const cols = Math.max(1, Math.min(tiles, Math.floor((W - 2 * PAD + GAP) / (NODE_MIN + GAP))));
  const nodeW = Math.min(NODE_MAX, (W - 2 * PAD - (cols - 1) * GAP) / cols);
  const gridX = (W - (cols * nodeW + (cols - 1) * GAP)) / 2;
  const perRow = Math.max(1, Math.min(BAYS_PER_ROW, Math.floor((nodeW - 24 + BAY_GAP) / (BAY_W + BAY_GAP))));
  const mostDisks = Math.max(1, ...shown.map((n) => Math.min(n.disks.length, diskLimit)));
  const bayRows = Math.ceil(mostDisks / perRow);
  const nodeH = HEAD_H + bayRows * (BAY_H + BAY_GAP) - BAY_GAP + FOOT_H + 6;

  // The bus says "these hosts hold copies of each other's data": only claim that when the pools
  // report more than one copy and there is more than one host.
  const showBus = nodes.length > 1 && !!replicaSizes?.some((s) => s > 1);
  const busH = showBus ? BUS_H : 0;
  const rows = Math.ceil(tiles / cols);
  const H = rows * (nodeH + busH) + (rows - 1) * ROW_GAP + 2 * PAD;
  const pos = (i: number) => ({
    x: gridX + (i % cols) * (nodeW + GAP),
    y: PAD + Math.floor(i / cols) * (nodeH + busH + ROW_GAP) + busH,
  });

  let bus = "";
  if (showBus) {
    const trunkX = Math.max(2, gridX - 7);
    for (let r = 0; r < rows; r++) {
      const first = r * cols;
      const last = Math.min(tiles, first + cols) - 1;
      const busY = pos(first).y - BUS_H / 2;
      const from = rows > 1 ? trunkX : pos(first).x + nodeW / 2;
      bus += `M${from} ${busY}H${pos(last).x + nodeW / 2}`;
      for (let i = first; i <= last; i++) bus += `M${pos(i).x + nodeW / 2} ${busY}v${BUS_H / 2}`;
    }
    if (rows > 1) bus += `M${trunkX} ${pos(0).y - BUS_H / 2}V${pos((rows - 1) * cols).y - BUS_H / 2}`;
  }

  const replication = replicationText(replicaSizes);
  const name = clusterName || "Storage cluster";
  const summary =
    `${name}: ${plural(nodes.length, "host")}, ${plural(allDisks.length, "disk")}; ` +
    `${totals.up} up and in, ${totals.out} up but out, ${totals.down} down. ${replication}.` +
    (rebuilding ? " Rebuilding data." : "");

  return (
    <figure className={`ag ag-hero ${className}`.trim()}>
      <figcaption className="ag-hero-head">
        <div>
          <div className="ag-hero-title">{name}</div>
          <div className="ag-hero-sub">
            {plural(nodes.length, "host")} · {plural(allDisks.length, "disk")} · {replication}
            {rebuilding && <strong className="ag-hero-rebuild"> · Rebuilding data</strong>}
          </div>
        </div>
        {health !== undefined && <HealthIndicator health={health} />}
      </figcaption>

      <div ref={ref} className="ag-hero-stage">
        <svg className="ag-hero-svg" width="100%" viewBox={`0 0 ${W} ${H}`} role="img" aria-label={summary}>
          <defs>
            <linearGradient id={chassis} x1="0" y1="0" x2="0" y2="1">
              <stop offset="0" className="ag-stop-face-top" />
              <stop offset="1" className="ag-stop-face-bottom" />
            </linearGradient>
          </defs>
          {showBus && (
            <g data-testid="replication-bus">
              <path className="ag-bus" d={bus} />
              {rebuilding && <path className="ag-bus-flow" d={bus} />}
            </g>
          )}
          {shown.map((node, i) => {
            const { x, y } = pos(i);
            const disks =
              node.disks.length > diskLimit
                ? [...node.disks].sort(
                    (a, b) => STATE_META[diskState(a)].rank - STATE_META[diskState(b)].rank,
                  )
                : node.disks;
            const drawn = disks.slice(0, diskLimit);
            const n = countStates(node.disks);
            const rowW = Math.min(perRow, drawn.length) * (BAY_W + BAY_GAP) - BAY_GAP;
            const trouble = n.down ? "fail" : n.out ? "warn" : null;
            const foot =
              (trouble ? problemText(n) : `${n.up}/${node.disks.length} up`) +
              (node.disks.length > drawn.length ? ` · +${node.disks.length - drawn.length} more` : "");
            return (
              <g key={node.host ?? "unreported"} className="ag-node" transform={`translate(${x} ${y})`}>
                <rect className="ag-node-shadow" x="6" y="8" width={nodeW - 12} height={nodeH - 4} rx="16" />
                <rect
                  className="ag-node-body"
                  width={nodeW}
                  height={nodeH}
                  rx="16"
                  fill={`url(#${chassis})`}
                />
                <text className="ag-node-name" x={nodeW / 2} y="23" textAnchor="middle">
                  <title>{hostName(node)}</title>
                  {shortHost(hostName(node), Math.floor((nodeW - 20) / 7.6))}
                </text>
                {drawn.map((disk, j) => (
                  <Bay
                    key={disk.id}
                    disk={disk}
                    x={(nodeW - rowW) / 2 + (j % perRow) * (BAY_W + BAY_GAP)}
                    y={HEAD_H + Math.floor(j / perRow) * (BAY_H + BAY_GAP)}
                  />
                ))}
                <text className="ag-node-foot" x={nodeW / 2} y={nodeH - 12} textAnchor="middle">
                  {clip(foot, Math.floor((nodeW - 12) / 6.4))}
                </text>
              </g>
            );
          })}
          {hidden.length > 0 &&
            (() => {
              const { x, y } = pos(shown.length);
              const problems = problemText(countStates(hidden.flatMap((n) => n.disks)));
              return (
                <g className="ag-node" transform={`translate(${x} ${y})`}>
                  <rect className="ag-node-more" width={nodeW} height={nodeH} rx="16" />
                  <text className="ag-node-name" x={nodeW / 2} y={nodeH / 2} textAnchor="middle">
                    +{plural(hidden.length, "host")}
                  </text>
                  <text className="ag-node-foot" x={nodeW / 2} y={nodeH / 2 + 20} textAnchor="middle">
                    {problems || "no disks down"}
                  </text>
                </g>
              );
            })()}
        </svg>
      </div>

      <ul className="ag-legend" aria-label="Disk states">
        {(["up", "out", "down"] as const).map((state) => (
          <li key={state}>
            <StatusGlyph tone={STATE_META[state].tone} size={12} />
            <span>
              {totals[state]} {STATE_META[state].word}
            </span>
          </li>
        ))}
      </ul>

      <ul className="ag-sr-only" aria-label="Disks by host">
        {nodes.map((node) => (
          <li key={node.host ?? "unreported"}>
            {hostName(node)}: {node.disks.map(diskDescription).join("; ") || "no disks"}
          </li>
        ))}
      </ul>
    </figure>
  );
}
