// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { fireEvent, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import Ceph from "./Ceph";

const TiB = 1024 ** 4;

function route(url: string) {
  const data =
    url === "/clusters"
      ? [{ id: "c1", backend_id: "bkd_ceph_lab", name: "lab", health: "ok" }]
      : url === "/backends/summary"
        ? [{ backend_id: "bkd_ceph_lab", backend_type: "ceph" }]
        : url === "/pools"
          ? // max_bytes is the space still available on Ceph, so used / max_bytes would read 800%.
            [{ id: "p1", cluster_id: "c1", name: "rbd-a", kind: "rbd", used_bytes: 40 * TiB, max_bytes: 5 * TiB, health: "ok" }]
          : url === "/ceph/df"
            ? { pools: [{ id: 1, name: "rbd-a", stats: { stored: 13 * TiB, max_avail: 5 * TiB, percent_used: 0.22 } }] }
            : url === "/ceph/osd-tree"
              ? {
                  nodes: [
                    { id: -1, type: "root", name: "default", children: [-2] },
                    { id: -2, type: "host", name: "node01", children: [0] },
                    { id: 0, type: "osd", name: "osd.0", status: "up" },
                  ],
                }
              : url === "/ceph/osd-df"
                ? { nodes: [{ id: 0, name: "osd.0", status: "up", utilization: 91, pgs: 33, device_class: "nvme" }] }
                : [];
  return Promise.resolve({ data });
}

describe("Ceph", () => {
  beforeEach(() => {
    stubFetch();
    vi.spyOn(http, "get").mockImplementation(route as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders without a live gateway", async () => {
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
    const { container } = renderView(<Ceph />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });

  it("takes pool fill from ceph df, never from used / max_bytes", async () => {
    renderView(<Ceph />);
    const list = await screen.findByRole("list", { name: "Pool usage" });
    expect(await within(list).findByText("22%")).toBeInTheDocument();
    expect(screen.queryByText(/800%|100%/)).toBeNull();
  });

  it("shows no placement-group count before Ceph reports one", async () => {
    renderView(<Ceph />);
    await flushQueries();
    expect(screen.getByText("No PG state data.")).toBeInTheDocument();
    expect(screen.queryByText("1 PGs")).toBeNull();
    expect(screen.queryByText("0 B/s")).toBeNull();
  });

  it("collapses the OSD tree and says when the fullest OSD is nearly full", async () => {
    renderView(<Ceph />);
    const host = await screen.findByRole("button", { name: /host node01/ });
    expect(screen.getByText("33 PGs · nvme")).toBeInTheDocument();
    expect(screen.getByText(/osd\.0 · avg 0\.0% · nearly full/)).toBeInTheDocument();
    fireEvent.click(host);
    expect(host).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByText("33 PGs · nvme")).toBeNull();
  });
});
