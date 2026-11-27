// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { fireEvent, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import Observatory from "./Observatory";

const TiB = 1024 ** 4;
const summary = {
  raw_capacity_bytes: 100 * TiB,
  used_capacity_bytes: 40 * TiB,
  available_capacity_bytes: 60 * TiB,
  used_capacity_percent: 40,
  clusters: 2,
  pools: 2,
  volumes: 3,
  snapshots: 0,
  buckets: 0,
  backups: 0,
  client_io: { read_ops_total: 10, write_ops_total: 5, read_bytes_total: 1024, write_bytes_total: 2048 },
  recovery: { pg_recovering: 0, pg_backfilling: 0, objects_degraded: 0, objects_misplaced: 0, objects_unfound: 0 },
};
const clusters = [
  { id: "c1", backend_id: "bkd_ceph_lab", name: "lab", health: "ok" },
  { id: "c2", backend_id: "bkd_zfs", name: "zfs", health: "ok" },
];
const backends = [
  { backend_id: "bkd_ceph_lab", backend_type: "ceph", raw_capacity_bytes: 100 * TiB, used_capacity_bytes: 40 * TiB },
  { backend_id: "bkd_zfs", backend_type: "zfs", raw_capacity_bytes: 0, used_capacity_bytes: 0 },
];
// On Ceph, max_bytes is the space still available: used / max_bytes would read 800%.
const pools = [
  { id: "p1", cluster_id: "c1", name: "rbd-a", kind: "rbd", used_bytes: 40 * TiB, max_bytes: 5 * TiB, health: "ok" },
  { id: "p2", cluster_id: "c2", name: "tank", kind: "zpool", used_bytes: 3 * TiB, max_bytes: 4 * TiB, health: "ok" },
];
const cephDf = { pools: [{ id: 1, name: "rbd-a", stats: { stored: 13 * TiB, max_avail: 5 * TiB, percent_used: 0.22 } }] };

function route(url: string) {
  const data = url.startsWith("/metrics/summary")
    ? summary
    : url === "/clusters"
      ? clusters
      : url === "/backends/summary"
        ? backends
        : url === "/pools"
          ? pools
          : url === "/ceph/df"
            ? cephDf
            : url.startsWith("/metrics/forecast")
              ? { days_to_full: null, growth_bytes_per_day: 0 }
              : url.startsWith("/io/heatmap")
                ? { status: "unavailable", detail: "agent not configured", series: [] }
                : [];
  return Promise.resolve({ data });
}

/** A 2D context that records every colour the page paints with. */
function recordingContext(colours: string[]) {
  const state: Record<string, unknown> = {};
  return new Proxy(state, {
    get: (target, key: string) => (key in target ? target[key] : () => {}),
    set: (target, key: string, value) => {
      if (key === "fillStyle" || key === "strokeStyle") colours.push(String(value));
      target[key] = value;
      return true;
    },
  }) as unknown as CanvasRenderingContext2D;
}

const TOKENS = ["surface-2", "surface-3", "line-strong", "ink", "ink-2", "blue", "series-2", "warn", "fail"];

describe("Observatory", () => {
  beforeEach(() => {
    stubFetch();
    vi.spyOn(http, "get").mockImplementation(route as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    document.documentElement.removeAttribute("style");
  });

  it("renders without a live gateway", async () => {
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
    const { container } = renderView(<Observatory />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });

  it("takes Ceph pool fill from ceph df and other pools from their total", async () => {
    renderView(<Observatory />);
    const list = await screen.findByRole("list", { name: "Pool usage" });
    expect(await within(list).findByText("22%")).toBeInTheDocument();
    expect(within(list).getByText("75%")).toBeInTheDocument();
    expect(within(list).queryByText("800%")).toBeNull();
    expect(screen.queryByText(/800%/)).toBeNull();
    expect(screen.getByText(/Fullest pool tank is 75% full/)).toBeInTheDocument();
  });

  it("does not draw a fill level for a backend that reported no capacity", async () => {
    renderView(<Observatory />);
    const list = await screen.findByRole("list", { name: "Backend capacity" });
    expect(within(list).getByText("Fill unknown")).toBeInTheDocument();
    expect(within(list).getByText("usage not reported")).toBeInTheDocument();
  });

  it("marks read and write like the chart series, not with state colours", async () => {
    const { container } = renderView(<Observatory />);
    await flushQueries();
    expect(container.querySelector(".obs-key-read")).not.toBeNull();
    expect(container.querySelector(".obs-key-write")).not.toBeNull();
    expect(container.querySelector(".obs-figures [style]")).toBeNull();
  });

  it("never skips a heading level", async () => {
    renderView(<Observatory />);
    await screen.findByText("Heatmap unavailable");
    const levels = screen.getAllByRole("heading").map((h) => Number(h.tagName.slice(1)));
    expect(levels[0]).toBe(1);
    levels.slice(1).forEach((level, i) => expect(level - levels[i]).toBeLessThanOrEqual(1));
  });

  it("paints both canvas views only with the theme's colours", async () => {
    TOKENS.forEach((t) => document.documentElement.style.setProperty(`--a-${t}`, `token(${t})`));
    const colours: string[] = [];
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockImplementation((() => recordingContext(colours)) as never);
    vi.stubGlobal("matchMedia", () => ({ matches: true }));
    renderView(<Observatory />);
    await screen.findByRole("list", { name: "Pool usage" });
    await screen.findByText("22%");

    for (const [button, name] of [
      ["Orbital", /Orbital view.*40% used/],
      // The Ceph pool is sized and filled from ceph df, the zpool from its own total.
      ["Treemap", /^Treemap of pools\. rbd-a 22% full, tank 75% full\.$/],
    ] as const) {
      colours.length = 0;
      fireEvent.click(screen.getByRole("button", { name: button }));
      expect(screen.getByRole("button", { name: button })).toHaveAttribute("aria-pressed", "true");
      expect(await screen.findByRole("img", { name })).toBeInTheDocument();
      expect(colours.length).toBeGreaterThan(0);
      colours.forEach((c) => expect(c).toMatch(/^token\(/));
    }
    // 75% is where capacity turns amber.
    expect(colours).toContain("token(warn)");
  });
});
