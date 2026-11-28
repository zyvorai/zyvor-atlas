// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen } from "@testing-library/react";
import { flushQueries, renderView, stubFetch } from "../../test/renderView";
import { http } from "../../api/client";
import { ConfirmHost } from "../../ui/confirm";
import PlanDetail from "./PlanDetail";

const PLAN = {
  id: "plan_1", tenant_id: "t", name: "billing-pg", source_id: "src_1",
  readiness_score: 80, rollback_window_secs: 3600, state: "validated",
};

// usePlan/useSource expect a single object, not a list — unlike every other view under test
// here, PlanDetail dereferences fields (plan.state, source?.name) straight off the response,
// so a bare `[]` (the generic list-endpoint default) makes it through the `if (!plan)` guard
// and crashes on `plan.state`. Shape the plan-detail response realistically instead. Outside
// a <Route>, useParams() never resolves `id`, so this also matches the empty-id request
// (`/databridge/plans/`) PlanDetail issues when mounted standalone like this.
function servePlan(plan: object) {
  vi.spyOn(http, "get").mockImplementation((url: string) =>
    Promise.resolve({
      data: /\/databridge\/plans\/[^/]*$/.test(url) ? plan : [],
    }) as never,
  );
}

describe("PlanDetail", () => {
  beforeEach(() => {
    stubFetch();
    servePlan(PLAN);
  });
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders without a live gateway", async () => {
    const { container } = renderView(<PlanDetail />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });

  it("asks before cutover and sends nothing until confirmed", async () => {
    servePlan({ ...PLAN, assessment: { score: 40, risk: "high", blockers: ["no primary key"] } });
    const post = vi.spyOn(http, "post").mockResolvedValue({ data: {} } as never);
    renderView(
      <>
        <PlanDetail />
        <ConfirmHost />
      </>,
    );
    fireEvent.click(await screen.findByRole("button", { name: "Run" }));
    const dialog = await screen.findByRole("dialog", { name: /Run cutover\?/ });
    expect(dialog.textContent).toContain(
      "Assessment still lists 1 blocker(s) (see below) — cutover will proceed anyway. Switches live traffic to the edge database. This is the point of no return short of a rollback within the window.",
    );
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await flushQueries();
    expect(post).not.toHaveBeenCalled();
  });

  it("asks before rollback and sends nothing until confirmed", async () => {
    servePlan({ ...PLAN, state: "cutover_complete" });
    const post = vi.spyOn(http, "post").mockResolvedValue({ data: {} } as never);
    renderView(
      <>
        <PlanDetail />
        <ConfirmHost />
      </>,
    );
    fireEvent.click(await screen.findByRole("button", { name: "Rollback" }));
    const dialog = await screen.findByRole("dialog", { name: /Roll back this migration\?/ });
    expect(dialog.textContent).toContain(
      "Reverts cutover and switches traffic back to the source database. This cannot be undone.",
    );
    expect(screen.getByRole("button", { name: "Roll back" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await flushQueries();
    expect(post).not.toHaveBeenCalled();
  });
});
