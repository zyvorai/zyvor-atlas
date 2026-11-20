// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent, screen, waitFor } from "@testing-library/react";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import OpsAdvisor from "./OpsAdvisor";

const ADVISOR = {
  mode: "local", risk_score: 35, risk_level: "moderate", summary: "s",
  evidence: {
    capacity_used_percent: 10, days_to_full: null, open_alerts: 1, critical_alerts: 0,
    warning_alerts: 1, failed_jobs_15m: 0, degraded_objects: 0, unfound_objects: 0, alert_titles: [],
  },
  actions: [], warnings: [], can_execute: false,
};
const INCIDENTS = { generated_at: "now", count: 0, incidents: [], can_execute: false };
const anomalies = (sensitivity: number) => ({
  generated_at: "now", window_minutes: 360, sample_count: 10,
  telemetry_status: "fresh" as const, latest_sample_age_minutes: 2, sensitivity,
  model: "robust_median_mad_v1", anomalies: [], warnings: [], can_execute: false,
});

describe("OpsAdvisor", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders without a live gateway", async () => {
    stubFetch();
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
    const { container } = renderView(<OpsAdvisor />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });

  it("re-fetches only /ai/anomalies (not the whole analysis) when the sensitivity chip changes", async () => {
    stubFetch();
    const get = vi.spyOn(http, "get").mockImplementation((url: string) =>
      Promise.resolve({
        data: url.includes("/ai/incidents") ? INCIDENTS : anomalies(3.5),
      }) as never,
    );
    const post = vi.spyOn(http, "post").mockResolvedValue({ data: ADVISOR } as never);

    renderView(<OpsAdvisor />);
    fireEvent.submit(screen.getByLabelText("Advisor question").closest("form")!);
    await waitFor(() => expect(screen.getByText(/sensitivity 3\.5/)).toBeInTheDocument());
    expect(post).toHaveBeenCalledTimes(1);

    get.mockImplementation(() => Promise.resolve({ data: anomalies(3) }) as never);
    fireEvent.click(screen.getByRole("button", { name: "Sensitive" }));
    await waitFor(() => expect(screen.getByText(/sensitivity 3(?!\.5)/)).toBeInTheDocument());

    // The full analyze() (advisor + incidents) must not re-run for a sensitivity-only change.
    expect(post).toHaveBeenCalledTimes(1);
  });
});
