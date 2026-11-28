// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import { Backends, Cluster, Kubernetes, Metrics, Policies } from "./Simple";

const TiB = 1024 ** 4;

function route(url: string) {
  const data =
    url === "/clusters"
      ? [
          { id: "c1", backend_id: "bkd_ceph_lab", name: "lab", health: "ok" },
          { id: "c2", backend_id: "bkd_zfs", name: "zfs", health: "ok" },
        ]
      : url === "/backends/summary"
        ? [
            { backend_id: "bkd_ceph_lab", backend_type: "ceph", raw_capacity_bytes: 10 * TiB, used_capacity_bytes: 8 * TiB, clusters: 1, volumes: 7 },
            { backend_id: "bkd_zfs", backend_type: "zfs", raw_capacity_bytes: 0, used_capacity_bytes: 0, clusters: 1, volumes: 0 },
          ]
        : url === "/backends"
          ? [{ id: "bkd_ceph_lab", name: "lab", backend_type: "ceph", mode: "real", status: "active" }]
          : url === "/pools"
            ? [
                // Ceph reports available space in max_bytes; ZFS reports the pool's total size.
                { id: "p1", cluster_id: "c1", name: "rbd-a", kind: "rbd", used_bytes: 40 * TiB, max_bytes: 5 * TiB, health: "ok" },
                { id: "p2", cluster_id: "c2", name: "tank", kind: "zpool", used_bytes: 3 * TiB, max_bytes: 4 * TiB, health: "ok" },
              ]
            : [];
  return Promise.resolve({ data });
}

describe("Simple views", () => {
  beforeEach(() => {
    stubFetch();
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it.each([
    ["Policies", Policies],
    ["Backends", Backends],
    ["Kubernetes", Kubernetes],
    ["Cluster", Cluster],
    ["Metrics", Metrics],
  ] as const)("%s renders without a live gateway", async (_name, View) => {
    const { container } = renderView(<View />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });

  it("Cluster never presents Ceph's available space as a pool's maximum", async () => {
    vi.spyOn(http, "get").mockImplementation(route as never);
    renderView(<Cluster />);
    const ceph = (await screen.findByText("rbd-a")).closest("tr")!;
    const table = ceph.closest("table")!;
    const headers = within(table).getAllByRole("columnheader").map((h) => h.textContent);
    expect(headers).toEqual(["Name", "Kind", "Used", "Total", "Available", "Fill", "Health"]);
    const cells = (row: HTMLElement) => within(row).getAllByRole("cell").map((c) => c.textContent);
    await vi.waitFor(() =>
      expect(cells(ceph).slice(2, 6)).toEqual(["40.0 TiB", "—", "5.0 TiB", "—"]),
    );
    expect(cells(screen.getByText("tank").closest("tr")!).slice(2, 6)).toEqual(["3.0 TiB", "4.0 TiB", "—", "75%"]);
  });

  it("Backends draws used of total and leaves an unsurveyed backend unfilled", async () => {
    vi.spyOn(http, "get").mockImplementation(route as never);
    renderView(<Backends />);
    const list = await screen.findByRole("list", { name: "Backend capacity" });
    expect(within(list).getByText("80%")).toBeInTheDocument();
    expect(within(list).getByText("Fill unknown")).toBeInTheDocument();
    expect(await screen.findByRole("columnheader", { name: "Volumes" })).toBeInTheDocument();
    expect(screen.getByText("7")).toBeInTheDocument();
  });
});
