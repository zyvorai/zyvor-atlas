// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { screen, within } from "@testing-library/react";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import ProtectionStatus from "./ProtectionStatus";

const row = (volume_id: string, verdict: string) => ({
  volume_id,
  volume_name: volume_id,
  storage_backend: "ceph",
  rpo_target_source: "none",
  rto_note: "",
  verdict,
  verdict_reasons: [`${verdict} because`],
});

describe("ProtectionStatus", () => {
  beforeEach(() => {
    stubFetch();
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders without a live gateway", async () => {
    const { container } = renderView(<ProtectionStatus />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });

  it("claims no counts while the verdicts are still loading", () => {
    vi.spyOn(http, "get").mockReturnValue(new Promise(() => {}) as never);
    renderView(<ProtectionStatus />);
    expect(screen.getByText("Loading protection status…")).toBeInTheDocument();
    expect(screen.queryByText("No volumes to protect yet.")).not.toBeInTheDocument();
    const summary = screen.getByRole("region", { name: "Verdict summary" });
    expect(within(summary).queryByText("0")).not.toBeInTheDocument();
  });

  it("says there is nothing to protect once the list is known to be empty", async () => {
    renderView(<ProtectionStatus />);
    expect(await screen.findByRole("heading", { name: "No volumes to protect yet" })).toBeInTheDocument();
  });

  it("counts verdicts in words and in one proportional bar", async () => {
    vi.spyOn(http, "get").mockResolvedValue({
      data: [row("a", "healthy"), row("b", "at_risk"), row("c", "healthy")],
    } as never);
    const { container } = renderView(<ProtectionStatus />);
    expect(await screen.findByText("3 volumes. Worst: 1 at risk.")).toBeInTheDocument();
    const items = within(screen.getByRole("region", { name: "Verdict summary" })).getAllByRole("listitem");
    expect(items.map((li) => li.textContent)).toEqual(["0Critical", "1At risk", "0Rebuilding", "0Degraded", "2Healthy"]);
    const segments = [...container.querySelectorAll<HTMLElement>(".ps-seg")];
    expect(segments.map((s) => [s.dataset.verdict, s.style.flexGrow])).toEqual([
      ["at_risk", "1"],
      ["healthy", "2"],
    ]);
    expect(screen.getByTitle("at_risk because")).toHaveTextContent("At risk");
  });
});
