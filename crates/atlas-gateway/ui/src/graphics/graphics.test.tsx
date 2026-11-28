// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import {
  CapacityRing,
  ClusterHero,
  EmptyState,
  HealthIndicator,
  PoolUsageBars,
  SpotIllustration,
  TrendChart,
  formatBytesPerSecond,
  replicationText,
  type HeroNode,
  type SpotKind,
  type TrendSeries,
} from "./index";
import { shortHost } from "./ClusterHero";
import { segments } from "./TrendChart";

const GiB = 1024 ** 3;

const disk = (n: number, up = true, inCluster = true) => ({
  id: `c/${n}`,
  label: `osd.${n}`,
  up,
  inCluster,
  usedBytes: up ? 25 * GiB : null,
  capacityBytes: 100 * GiB,
});

const nodes: HeroNode[] = [
  { host: "node01", disks: [disk(0), disk(1)] },
  { host: "node02", disks: [disk(2), disk(3, false)] },
  { host: "node03", disks: [disk(4), disk(5, true, false)] },
];

describe("ClusterHero", () => {
  it("shows an empty state, and no invented counts, without topology", () => {
    const { container, rerender } = render(<ClusterHero />);
    expect(screen.getByRole("status")).toHaveTextContent("No disks reported");
    expect(container.textContent).not.toMatch(/\d/);
    rerender(<ClusterHero nodes={[]} />);
    expect(screen.getByRole("status")).toHaveTextContent("No disks reported");
    rerender(<ClusterHero loading />);
    expect(screen.getByRole("status")).toHaveTextContent("Loading");
  });

  it("draws one bay per disk and describes the topology in text", () => {
    const { container } = render(<ClusterHero nodes={nodes} clusterName="lab" replicaSizes={[3]} />);
    expect(container.querySelectorAll(".ag-bay")).toHaveLength(6);
    expect(screen.getByRole("img")).toHaveAccessibleName(
      "lab: 3 hosts, 6 disks; 4 up and in, 1 up but out, 1 down. Replica size 3.",
    );
    const list = screen.getByRole("list", { name: "Disks by host" });
    expect(list).toHaveTextContent("node02: osd.2, up and in, 25 GiB of 100 GiB used");
    expect(list).toHaveTextContent("osd.3, down, usage not reported");
  });

  it("marks disk state by glyph shape and words, not only colour", () => {
    const { container } = render(<ClusterHero nodes={nodes} />);
    expect(container.querySelector('.ag-bay[data-state="down"] [data-glyph="fail"]')).not.toBeNull();
    expect(container.querySelector('.ag-bay[data-state="out"] [data-glyph="warn"]')).not.toBeNull();
    expect(container.querySelector('.ag-bay[data-state="up"] [data-glyph="ok"]')).not.toBeNull();
    expect(screen.getByRole("list", { name: "Disk states" })).toHaveTextContent("1 down");
    expect(container.querySelector("svg")).toHaveTextContent("1 down");
  });

  it("draws replication only when more than one copy on more than one host is reported", () => {
    const { rerender } = render(<ClusterHero nodes={nodes} />);
    expect(screen.queryByTestId("replication-bus")).toBeNull();
    expect(screen.getByRole("img")).toHaveAccessibleName(/Replication not reported/);
    rerender(<ClusterHero nodes={nodes} replicaSizes={[1]} />);
    expect(screen.queryByTestId("replication-bus")).toBeNull();
    rerender(<ClusterHero nodes={nodes.slice(0, 1)} replicaSizes={[3]} />);
    expect(screen.queryByTestId("replication-bus")).toBeNull();
    rerender(<ClusterHero nodes={nodes} replicaSizes={[2, 3]} />);
    expect(screen.getByTestId("replication-bus")).toBeInTheDocument();
  });

  it("animates replication flow only while rebuilding, and says so", () => {
    const { container, rerender } = render(<ClusterHero nodes={nodes} replicaSizes={[3]} />);
    expect(container.querySelector(".ag-bus-flow")).toBeNull();
    rerender(<ClusterHero nodes={nodes} replicaSizes={[3]} rebuilding />);
    expect(container.querySelector(".ag-bus-flow")).not.toBeNull();
    expect(screen.getByText(/· Rebuilding data/)).toBeInTheDocument();
  });

  it("never hides a failed disk when truncating", () => {
    const many: HeroNode[] = [
      { host: "a", disks: [disk(10)] },
      { host: "b", disks: [disk(11)] },
      { host: "z", disks: [disk(12), disk(13), disk(14, false)] },
    ];
    const { container } = render(<ClusterHero nodes={many} maxNodes={1} maxDisksPerNode={1} />);
    expect(container.querySelectorAll(".ag-bay")).toHaveLength(1);
    expect(container.querySelector('.ag-bay[data-state="down"]')).not.toBeNull();
    expect(container.querySelector("svg")).toHaveTextContent("+2 hosts");
    expect(container.querySelector("svg")).toHaveTextContent("+2 more");
  });

  it("shortens host names without making neighbours look alike", () => {
    expect(shortHost("node01", 20)).toBe("node01");
    expect(shortHost("node01.rack.example", 12)).toBe("node01");
    expect(shortHost("storage-hostname-01.rack.example", 12)).toBe("storag…me-01");
    expect(shortHost("storage-hostname-02.rack.example", 12)).not.toBe(
      shortHost("storage-hostname-01.rack.example", 12),
    );
    expect(shortHost("192.168.100.200", 10)).toContain(".200");
  });

  it("words replica sizes without guessing", () => {
    expect(replicationText()).toBe("Replication not reported");
    expect(replicationText([1])).toBe("Replica size 1: no redundant copies");
    expect(replicationText([2, 3])).toBe("Replica size 2–3 across pools");
  });
});

describe("CapacityRing", () => {
  it("renders percent, bytes and a text alternative", () => {
    render(<CapacityRing usedBytes={25 * GiB} totalBytes={100 * GiB} />);
    expect(screen.getByRole("img")).toHaveAccessibleName("Capacity: 25% full, 25 GiB of 100 GiB used");
    expect(screen.getByText("75 GiB free")).toBeInTheDocument();
  });

  it.each([
    [undefined, undefined, "Capacity not reported"],
    [0, 0, "Capacity not reported"],
    [5 * GiB, null, "5 GiB used · total not reported"],
    [null, 100 * GiB, "100 GiB total · usage not reported"],
  ])("does not invent a percentage for used=%s total=%s", (used, total, text) => {
    const { container } = render(<CapacityRing usedBytes={used} totalBytes={total} />);
    expect(screen.getByText(text)).toBeInTheDocument();
    expect(container.textContent).not.toContain("%");
    expect(container.querySelector(".ag-ring-arc")).toBeNull();
  });

  it("words high fill levels and never rounds up to full", () => {
    const { container, rerender } = render(<CapacityRing usedBytes={75} totalBytes={100} />);
    expect(screen.getByText("Filling up")).toBeInTheDocument();
    rerender(<CapacityRing usedBytes={9996} totalBytes={10000} />);
    expect(screen.getByText("Nearly full")).toBeInTheDocument();
    expect(container.querySelector(".ag-ring-value")).toHaveTextContent("99%");
    rerender(<CapacityRing usedBytes={1} totalBytes={10000} />);
    expect(container.querySelector(".ag-ring-value")).toHaveTextContent("<1%");
  });
});

describe("PoolUsageBars", () => {
  const pools = [
    { id: "a", name: "small", usedBytes: 10, capacityBytes: 100 },
    { id: "b", name: "unknown", usedBytes: 10, availableBytes: 90 },
    { id: "c", name: "full", usedBytes: 90, capacityBytes: 100, health: "warn" as const },
    { id: "d", name: "ceph", usedBytes: 10, availableBytes: 90, percentUsed: 50 },
  ];

  it("has an empty state", () => {
    render(<PoolUsageBars pools={[]} />);
    expect(screen.getByRole("status")).toHaveTextContent("No pools reported");
  });

  it("sorts by fill and leaves unknown pools without a bar", () => {
    const { container } = render(<PoolUsageBars pools={pools} />);
    const rows = within(screen.getByRole("list", { name: "Pool usage" })).getAllByRole("listitem");
    expect(rows.map((r) => r.querySelector(".ag-pool-name")?.textContent)).toEqual([
      "full",
      "ceph",
      "small",
      "unknown",
    ]);
    expect(rows[0]).toHaveTextContent("Nearly full");
    expect(rows[0]).toHaveTextContent("Health: Warning");
    expect(rows[1]).toHaveTextContent("50%");
    expect(rows[3]).toHaveTextContent("Fill unknown");
    expect(rows[3]).toHaveTextContent("10 B used · 90 B available");
    expect(container.querySelectorAll(".ag-bar-fill")).toHaveLength(3);
  });

  it("limits rows and supports selection", () => {
    const onSelect = vi.fn();
    render(<PoolUsageBars pools={pools} limit={1} onSelect={onSelect} />);
    expect(screen.getByText("3 more pools not shown")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /full/ }));
    expect(onSelect).toHaveBeenCalledWith(expect.objectContaining({ id: "c" }));
  });
});

describe("TrendChart", () => {
  const t = (i: number) => Date.UTC(2026, 0, 1, 12, i);
  const series: TrendSeries[] = [
    { id: "read", label: "Read", points: [1024, 2048, null, 4096].map((v, i) => ({ t: t(i), v })) },
    { id: "write", label: "Write", points: [512, 512, 512, 512].map((v, i) => ({ t: t(i), v })) },
  ];

  it("breaks lines at gaps instead of interpolating", () => {
    expect(segments(series[0].points).map((r) => r.length)).toEqual([2, 1]);
    const spaced = [0, 1, 2, 30, 31].map((i) => ({ t: t(i), v: 1 }));
    expect(segments(spaced).map((r) => r.length)).toEqual([3, 2]);
    expect(segments(spaced, Infinity)).toHaveLength(1);
  });

  it.each([
    ["no series", undefined],
    ["one sample", [{ id: "r", label: "Read", points: [{ t: 0, v: 5 }] }]],
    ["only gaps", [{ id: "r", label: "Read", points: [{ t: 0, v: null }, { t: 1, v: null }] }]],
  ])("is honest with %s", (_name, input) => {
    const { container } = render(<TrendChart title="Throughput" series={input} />);
    expect(screen.getByRole("status")).toHaveTextContent("Not enough samples yet");
    expect(container.querySelector(".ag-line")).toBeNull();
  });

  it("offers a summary, a legend, a table and keyboard readout", () => {
    render(<TrendChart title="Throughput" series={series} formatValue={formatBytesPerSecond} />);
    expect(screen.getByRole("img").getAttribute("aria-label")).toMatch(
      /Read: 4 KiB\/s at .*range 1 KiB\/s to 4 KiB\/s\. Write: 512 B\/s/,
    );
    expect(screen.getByRole("table")).toHaveTextContent("no sample");
    const readout = document.querySelector(".ag-trend-readout") as HTMLElement;
    expect(readout).toHaveTextContent("Latest");
    expect(readout).toHaveTextContent("Read4 KiB/s");

    const plot = screen.getByRole("group", { name: /arrow keys/ });
    fireEvent.keyDown(plot, { key: "ArrowLeft" });
    expect(readout).toHaveTextContent("Readno sample");
    expect(screen.getByTestId("crosshair")).toBeInTheDocument();
    fireEvent.keyDown(plot, { key: "Home" });
    expect(readout).toHaveTextContent("Read1 KiB/s");
    fireEvent.keyDown(plot, { key: "Escape" });
    expect(screen.queryByTestId("crosshair")).toBeNull();
  });

  it("renders a bare sparkline", () => {
    const { container } = render(<TrendChart title="Writes" variant="sparkline" series={[series[1]]} />);
    expect(screen.getByRole("img").getAttribute("aria-label")).toContain("Writes. Write:");
    expect(container.querySelector("table")).toBeNull();
    expect(container.querySelector(".ag-axis")).toBeNull();
  });

  it("keeps all-zero data finite", () => {
    const zero = [{ id: "r", label: "Read", points: [0, 0].map((v, i) => ({ t: t(i), v })) }];
    const { container } = render(<TrendChart title="Idle" series={zero} />);
    expect(container.innerHTML).not.toMatch(/NaN|Infinity/);
  });
});

describe("HealthIndicator", () => {
  it.each([
    ["ok", "Healthy", "ok"],
    ["warn", "Warning", "warn"],
    ["rebuilding", "Rebuilding", "info"],
    ["critical", "Critical", "fail"],
    ["unknown", "Unknown", "unknown"],
    [undefined, "Not reported", "unknown"],
  ] as const)("renders %s as a word and a shape", (health, word, glyph) => {
    const { container } = render(<HealthIndicator health={health} label="Cluster" />);
    expect(screen.getByText(`Cluster: ${word}`)).toBeInTheDocument();
    expect(container.querySelector(`[data-glyph="${glyph}"]`)).not.toBeNull();
  });

  it("keeps an accessible name when icon-only", () => {
    render(<HealthIndicator health="degraded" detail="1 OSD down" iconOnly />);
    expect(screen.getByRole("img")).toHaveAccessibleName("Degraded. 1 OSD down");
  });
});

describe("illustrations", () => {
  const kinds: SpotKind[] = ["cluster", "volume", "pool", "snapshot", "backup", "bucket", "chart", "alert"];

  it.each(kinds)("draws %s as decorative art", (kind) => {
    const { container } = render(<SpotIllustration kind={kind} />);
    const svg = container.querySelector("svg")!;
    expect(svg).toHaveAttribute("aria-hidden", "true");
    expect(svg.querySelector(".ag-spot-float")?.childElementCount).toBeGreaterThan(0);
  });

  it("gives gradients unique ids and an optional name", () => {
    const { container } = render(
      <>
        <SpotIllustration kind="pool" title="Pools" />
        <SpotIllustration kind="pool" />
      </>,
    );
    const ids = [...container.querySelectorAll("linearGradient")].map((g) => g.id);
    expect(new Set(ids).size).toBe(ids.length);
    expect(screen.getByRole("img", { name: "Pools" })).toBeInTheDocument();
  });

  it("renders an empty state with an action", () => {
    render(
      <EmptyState art="volume" title="No volumes" description="Create one to start.">
        <button type="button">New volume</button>
      </EmptyState>,
    );
    expect(screen.getByRole("heading", { name: "No volumes" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "New volume" })).toBeInTheDocument();
  });
});
