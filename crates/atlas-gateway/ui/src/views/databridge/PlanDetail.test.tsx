// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushQueries, renderView, stubFetch } from "../../test/renderView";
import { http } from "../../api/client";
import PlanDetail from "./PlanDetail";

const PLAN = {
  id: "plan_1", tenant_id: "t", name: "billing-pg", source_id: "src_1",
  readiness_score: 80, rollback_window_secs: 3600, state: "validated",
};

describe("PlanDetail", () => {
  beforeEach(() => {
    stubFetch();
    // usePlan/useSource expect a single object, not a list — unlike every other view under test
    // here, PlanDetail dereferences fields (plan.state, source?.name) straight off the response,
    // so a bare `[]` (the generic list-endpoint default) makes it through the `if (!plan)` guard
    // and crashes on `plan.state`. Shape the plan-detail response realistically instead. Outside
    // a <Route>, useParams() never resolves `id`, so this also matches the empty-id request
    // (`/databridge/plans/`) PlanDetail issues when mounted standalone like this.
    vi.spyOn(http, "get").mockImplementation((url: string) =>
      Promise.resolve({
        data: /\/databridge\/plans\/[^/]*$/.test(url) ? PLAN : [],
      }) as never,
    );
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders without a live gateway", async () => {
    const { container } = renderView(<PlanDetail />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });
});
