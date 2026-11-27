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

  it("draws no capacity, throughput or OSDs it has not been given", async () => {
    renderView(<Overview />);
    await flushQueries();
    expect(screen.getByRole("img", { name: "Capacity not surveyed" })).toBeInTheDocument();
    expect(screen.getByText(/Collecting samples/)).toBeInTheDocument();
    expect(screen.getByText("No OSDs reported.")).toBeInTheDocument();
    expect(screen.queryByRole("list", { name: "OSD states" })).not.toBeInTheDocument();
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
