// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { fireEvent, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import Maintenance from "./Maintenance";

describe("Maintenance", () => {
  beforeEach(() => {
    stubFetch();
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders without a live gateway", async () => {
    const { container } = renderView(<Maintenance />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });

  it("does not call an upgrade safe before the pre-flight has answered", async () => {
    vi.spyOn(http, "get").mockReturnValue(new Promise(() => {}) as never);
    renderView(<Maintenance />);
    await flushQueries();
    expect(screen.getByText("Loading pre-flight…")).toBeInTheDocument();
    expect(screen.queryByText("Safe to upgrade.")).toBeNull();
  });

  it("says safe to upgrade once the pre-flight is ready", async () => {
    vi.spyOn(http, "get").mockImplementation(((url: string) =>
      Promise.resolve({
        data: url === "/upgrade/preflight" ? { ready: true, checks: [], blockers: [] } : [],
      })) as never);
    renderView(<Maintenance />);
    expect(await screen.findByText("Safe to upgrade.")).toBeInTheDocument();
  });

  it("pauses the job engine only after the confirmation", async () => {
    vi.spyOn(http, "get").mockImplementation(((url: string) =>
      Promise.resolve({ data: url === "/maintenance" ? { paused: false } : [] })) as never);
    const request = vi.spyOn(http, "request").mockResolvedValue({ data: {} } as never);
    const confirm = vi.fn(() => false);
    vi.stubGlobal("confirm", confirm);
    renderView(<Maintenance />);
    await screen.findByText("dispatching");
    fireEvent.click(screen.getByRole("button", { name: "Pause" }));
    await flushQueries();
    expect(confirm).toHaveBeenCalledWith(expect.stringContaining("New jobs will queue"));
    expect(request).not.toHaveBeenCalled();
  });
});
