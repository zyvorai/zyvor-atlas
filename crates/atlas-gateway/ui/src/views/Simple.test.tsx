// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import { Backends, Cluster, Kubernetes, Metrics, Policies } from "./Simple";

describe("Simple views", () => {
  beforeEach(() => {
    stubFetch();
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it.each([
    ["Policies", Policies],
    ["Backends", Backends],
    ["Kubernetes", Kubernetes],
    ["Cluster", Cluster],
    ["Metrics", Metrics],
  ] as const)("%s renders without a live gateway", async (_name, View) => {
    const { container } = renderView(<View />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });
});
