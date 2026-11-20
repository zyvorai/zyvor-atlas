// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
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
});
