// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
import { screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import FastStorage from "./FastStorage";

const nvme = {
  summary: { total: 2, scraped: 2, with_concerns: 1, worn_at_percent: 80, truncated: false },
  devices: [
    { devid: "a", host: "node01", dev: "nvme0n1", scraped: true, wear_percent: 92, available_spare_percent: 100, media_errors: 3, temperature_c: 41, concerns: ["92% of rated endurance used"] },
    { devid: "b", host: "node02", dev: "nvme0n1", scraped: true, wear_percent: 2, available_spare_percent: 100, media_errors: 0, temperature_c: 38, concerns: [] },
  ],
};
const sizing = {
  thresholds: { cores_per_nvme_device: 10, ram_base_gib: 16, ram_per_osd_gib: 5, note: "heuristics" },
  summary: { hosts: 1, undersized: 1, unknown: 0 },
  hosts: [{ host: "node01", nvme_devices: 2, osds: 4, needs_cpu_cores: 20, needs_memory_gib: 36, status: "undersized", has_cpu_cores: 8, has_memory_gib: 32, shortfalls: ["8 CPU cores, rule of thumb asks for 20"] }],
};
const stores = { summary: { total: 2, by_objectstore: { bluestore: 2 }, by_version: { "19.2.0": 1, "19.2.1": 1 }, mixed_versions: true }, osds: [] };

function route(ioai: unknown) {
  return (path: string) => {
    const data = path.startsWith("/ceph/nvme") ? nvme : path.startsWith("/ceph/sizing") ? sizing : path.startsWith("/ceph/osd-objectstore") ? stores : path.startsWith("/io/ai") ? ioai : [];
    return Promise.resolve({ data });
  };
}

describe("FastStorage", () => {
  beforeEach(() => stubFetch());
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("shows worn drives, undersized hosts and mixed versions from the gateway", async () => {
    vi.spyOn(http, "get").mockImplementation(route({ status: "ok", detail: null, devices: [] }) as never);
    renderView(<FastStorage />);
    await flushQueries();
    expect(await screen.findByText("node01:nvme0n1")).toBeInTheDocument();
    expect(screen.getByText("92%")).toBeInTheDocument();
    expect(screen.getByText("92% of rated endurance used")).toBeInTheDocument();
    expect(screen.getByText("undersized")).toBeInTheDocument();
    expect(screen.getByText("mixed versions")).toBeInTheDocument();
  });

  it("says an unreachable I/O agent was not assessed instead of looking healthy", async () => {
    vi.spyOn(http, "get").mockImplementation(
      route({ status: "unavailable", detail: "ATLAS_IO_AGENT_URL is not configured", devices: [] }) as never,
    );
    renderView(<FastStorage />);
    await flushQueries();
    expect(await screen.findByText(/Not assessed: ATLAS_IO_AGENT_URL is not configured/)).toBeInTheDocument();
    expect(screen.getByText(/not a healthy reading/)).toBeInTheDocument();
  });

  it("renders without a live gateway", async () => {
    vi.spyOn(http, "get").mockRejectedValue(new Error("offline"));
    const { container } = renderView(<FastStorage />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });
});
