// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { screen } from "@testing-library/react";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import Overview from "./Overview";

describe("Overview", () => {
  beforeEach(() => {
    stubFetch();
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders without a live gateway", async () => {
    const { container } = renderView(<Overview />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });

  it("draws no capacity, throughput or disks it has not been given", async () => {
    renderView(<Overview />);
    expect(await screen.findByRole("img", { name: /Capacity not reported/ })).toBeInTheDocument();
    expect(await screen.findByText("No disks reported")).toBeInTheDocument();
    expect(await screen.findByText("Not enough samples yet")).toBeInTheDocument();
    expect(screen.getByText("Client IOPS: not enough samples")).toBeInTheDocument();
  });

  it("takes a Ceph pool's fill from ceph df, not from used / max", async () => {
    const data: Record<string, unknown> = {
      "/clusters": [{ id: "c1", backend_id: "bkd_ceph_lab", name: "lab", health: "ok" }],
      "/pools": [
        { id: "pool_1", cluster_id: "c1", name: "rbd", kind: "rbd", used_bytes: 900, max_bytes: 1000, health: "ok" },
      ],
      "/ceph/df": { pools: [{ id: 1, name: "rbd", stats: { stored: 300, max_avail: 1000, percent_used: 0.23 } }] },
    };
    vi.spyOn(http, "get").mockImplementation(((url: string) => Promise.resolve({ data: data[url] ?? [] })) as never);
    renderView(<Overview />);
    expect(await screen.findByText("23%")).toBeInTheDocument();
    expect(screen.queryByText("90%")).not.toBeInTheDocument();
  });

  it("shows alert titles as text, never as markup", async () => {
    vi.spyOn(http, "get").mockImplementation(((url: string) =>
      Promise.resolve({
        data: url.startsWith("/alerts")
          ? [{ id: "a1", severity: "critical", title: "<img src=x onerror=alert(1)>", resource_id: "osd.1", state: "open" }]
          : url.startsWith("/clusters")
            ? [{ id: "c1", backend_id: "b", name: "lab", health: "critical" }]
            : [],
      })) as never);
    const { container } = renderView(<Overview />);
    expect(await screen.findByRole("heading", { level: 1, name: "Storage is critical." })).toBeInTheDocument();
    expect(await screen.findAllByText(/<img src=x onerror=alert\(1\)>/)).not.toHaveLength(0);
    expect(container.querySelector("img")).toBeNull();
  });
});
