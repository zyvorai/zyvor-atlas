// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { fireEvent, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import DR from "./DR";

const mirror = { id: "m1", pool: "rbd", image: "db", role: "secondary", state: "up", mode: "snapshot" };

describe("DR", () => {
  beforeEach(() => {
    stubFetch();
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders without a live gateway", async () => {
    const { container } = renderView(<DR />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });

  it("reports no preflight or data-plane verdict while loading", async () => {
    vi.spyOn(http, "get").mockReturnValue(new Promise(() => {}) as never);
    renderView(<DR />);
    await flushQueries();
    expect(screen.queryByText("blocked")).toBeNull();
    expect(screen.queryByText("unverified")).toBeNull();
  });

  it("names every form control", async () => {
    renderView(<DR />);
    await flushQueries();
    for (const name of ["Name", "Cluster FSID", "Secret ref", "Direction", "Block volume", "Peer", "Mode"]) {
      expect(screen.getByLabelText(name)).toBeInTheDocument();
    }
  });

  it("promotes only after the confirmation is accepted", async () => {
    vi.spyOn(http, "get").mockImplementation(((url: string) =>
      Promise.resolve({ data: url === "/dr/mirrors" ? [mirror] : [] })) as never);
    const request = vi.spyOn(http, "request").mockResolvedValue({ data: {} } as never);
    const confirm = vi.fn(() => false);
    vi.stubGlobal("confirm", confirm);
    renderView(<DR />);

    fireEvent.click(await screen.findByRole("button", { name: "Promote" }));
    await flushQueries();
    expect(confirm).toHaveBeenCalledWith("Promote rbd/db to primary on this cluster.");
    expect(request).not.toHaveBeenCalled();

    confirm.mockReturnValue(true);
    fireEvent.click(screen.getByRole("button", { name: "Promote" }));
    await vi.waitFor(() =>
      expect(request).toHaveBeenCalledWith(
        expect.objectContaining({ method: "post", url: "/dr/mirrors/m1/promote" }),
      ),
    );
  });
});
