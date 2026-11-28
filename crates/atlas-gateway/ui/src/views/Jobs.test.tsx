// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import Jobs from "./Jobs";

const job = (id: string, state: string, progress_percent: number) => ({
  id,
  state,
  progress_percent,
  job_type: "volume.create",
  requested_by: "admin",
  tenant_id: "t",
});

describe("Jobs", () => {
  beforeEach(() => {
    stubFetch();
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders without a live gateway", async () => {
    const { container } = renderView(<Jobs />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });

  it("colours progress by state from the theme, with the value exposed", async () => {
    vi.spyOn(http, "get").mockResolvedValue({
      data: [job("job_a", "failed", 60), job("job_b", "succeeded", 100)],
    } as never);
    renderView(<Jobs />);
    const bar = await screen.findByRole("progressbar", { name: "job_a progress" });
    expect(bar).toHaveAttribute("aria-valuenow", "60");
    expect(bar).toHaveAttribute("data-state", "failed");
    expect(screen.getByRole("progressbar", { name: "job_b progress" })).toHaveAttribute("data-state", "succeeded");
    // Only the width is inline; every colour comes from the stylesheet.
    expect((bar.firstElementChild as HTMLElement).getAttribute("style")).toBe("width: 60%;");
  });
});
