// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { screen, within } from "@testing-library/react";
import { Route, Routes } from "react-router-dom";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import PoolDetail from "./PoolDetail";

const GiB = 1024 * 1024 * 1024;

const pool = (over: object) => ({
  id: "pool_1",
  cluster_id: "c1",
  name: "rbd",
  kind: "rbd",
  used_bytes: 900 * GiB,
  max_bytes: 1000 * GiB,
  health: "ok",
  ...over,
});

const ceph: Record<string, unknown> = {
  "/clusters": [{ id: "c1", backend_id: "bkd_ceph_lab", name: "lab", health: "ok" }],
  "/backends/summary": [{ backend_id: "bkd_ceph_lab", backend_type: "ceph" }],
  "/pools": [pool({})],
};

function mount(data: Record<string, unknown>) {
  vi.spyOn(http, "get").mockImplementation(((url: string) => Promise.resolve({ data: data[url] ?? [] })) as never);
  return renderView(
    <Routes>
      <Route path="/pools/:id" element={<PoolDetail />} />
    </Routes>,
    "/pools/pool_1",
  );
}

describe("PoolDetail", () => {
  beforeEach(() => {
    stubFetch();
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders without a live gateway", async () => {
    const { container } = renderView(<PoolDetail />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });

  it("says when the pool is not in the inventory", async () => {
    mount({ "/pools": [] });
    expect(await screen.findByRole("heading", { level: 1, name: "Pool not found" })).toBeInTheDocument();
  });

  it("takes a Ceph pool's fill from ceph df, never from used / max", async () => {
    const { container } = mount({
      ...ceph,
      "/ceph/df": { pools: [{ id: 1, name: "rbd", stats: { stored: 300 * GiB, max_avail: 1000 * GiB, percent_used: 0.23 } }] },
    });
    expect(await screen.findByText("Healthy rbd pool, 23% full.")).toBeInTheDocument();
    expect(screen.getByText("300 GiB used · 1000 GiB available")).toBeInTheDocument();
    // max_bytes is the space still free on Ceph, so it is shown as that.
    expect(screen.getByText("Available").parentElement).toHaveTextContent("1000.0GiB");
    expect(container.textContent).not.toContain("90%");
    expect(container.textContent).not.toContain("100.0GiB");
    expect(container.querySelector('svg[preserveAspectRatio="none"]')).toBeNull();
  });

  it("leaves a Ceph fill level unknown without ceph df", async () => {
    const { container } = mount(ceph);
    expect(await screen.findByText("Healthy rbd pool, fill level not reported.")).toBeInTheDocument();
    expect(screen.getByText("Fill unknown")).toBeInTheDocument();
    expect(container.textContent).not.toContain("%");
  });

  it("divides used by max where max is the pool's size", async () => {
    mount({
      "/clusters": [{ id: "c1", backend_id: "bkd_zfs", name: "host", health: "ok" }],
      "/backends/summary": [{ backend_id: "bkd_zfs", backend_type: "zfs" }],
      "/pools": [pool({ name: "tank", kind: "zpool", used_bytes: 800 * GiB })],
    });
    expect(await screen.findByRole("img", { name: /tank: 80% full, 800 GiB of 1000 GiB used\. Filling up/ })).toBeInTheDocument();
    expect(screen.getByText("Healthy zpool pool, 80% full. Filling up.")).toBeInTheDocument();
    expect(screen.getByText("Free").parentElement).toHaveTextContent("200.0GiB");
  });

  it("lists exactly the cluster's OSDs, without repeating any to fill a grid", async () => {
    const osd = (id: number, cluster_id: string, up: boolean, in_cluster = true) => ({
      id,
      osd_num: id,
      cluster_id,
      up,
      in_cluster,
    });
    mount({ ...ceph, "/osds": [osd(1, "c1", false), osd(0, "c1", true), osd(7, "other", true)] });
    const list = await screen.findByRole("list", { name: "OSDs in this cluster" });
    const items = within(list).getAllByRole("listitem");
    expect(items.map((li) => li.textContent)).toEqual(["osd.0up, in", "osd.1down"]);
    expect(screen.getByText("2 OSDs")).toBeInTheDocument();
  });

  it("shows the pool's volumes with usage only where reported", async () => {
    const vol = (id: string, used_bytes: number | null) => ({
      id,
      name: id,
      pool_id: "pool_1",
      kind: "block",
      size_bytes: 4 * GiB,
      used_bytes,
      state: "bound",
      health: "ok",
    });
    mount({ ...ceph, "/volumes": [vol("with-usage", 1 * GiB), vol("without-usage", null)] });
    const used = (await screen.findByRole("button", { name: "with-usage" })).closest("tr")!;
    expect(used).toHaveTextContent("1.0 GiB · 25%");
    const unknown = screen.getByRole("button", { name: "without-usage" }).closest("tr")!;
    expect(unknown.querySelector(".at-mini")).toBeNull();
    expect(screen.queryByRole("list", { name: "OSDs in this cluster" })).not.toBeInTheDocument();
  });
});
