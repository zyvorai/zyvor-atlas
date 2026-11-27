// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import IoHeatmap from "./IoHeatmap";

const column = (start_unix: number, count: number, peak_us: number) => ({ start_unix, count, peak_us });

describe("IoHeatmap", () => {
  beforeEach(() => stubFetch());
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("bands each minute without inline colours and describes the row in words", async () => {
    vi.spyOn(http, "get").mockResolvedValue({
      data: {
        status: "ok",
        series: [
          {
            device: "nvme0n1",
            op: "read",
            columns: [column(0, 0, 0), column(60, 4, 512), column(120, 2, 4096), column(180, 1, 32768)],
          },
        ],
      },
    } as never);
    const { container } = renderView(<IoHeatmap />);
    const row = await screen.findByRole("img", { name: "nvme0n1 read latency: 7 I/Os over 4 minutes, peak 33 ms" });
    expect([...row.children].map((c) => c.getAttribute("data-band"))).toEqual(["none", "fast", "slow", "slower"]);
    expect(container.querySelector("[style]")).toBeNull();
    expect(screen.getByRole("list", { name: "Peak latency per minute" })).toHaveTextContent("10 ms or more");
  });

  it("says an unconfigured agent is unavailable", async () => {
    vi.spyOn(http, "get").mockResolvedValue({
      data: { status: "unavailable", detail: "ATLAS_IO_AGENT_URL is not configured", series: [] },
    } as never);
    renderView(<IoHeatmap />);
    expect(await screen.findByText("Heatmap unavailable")).toBeInTheDocument();
    expect(screen.getByText("ATLAS_IO_AGENT_URL is not configured")).toBeInTheDocument();
  });
});
