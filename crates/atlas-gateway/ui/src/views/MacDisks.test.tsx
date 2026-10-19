// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { screen } from "@testing-library/react";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import MacDisks from "./MacDisks";

const devices = [
  { device: "disk0", internal: true, bus: "Apple Fabric", media_name: "APPLE SSD", smart: "Verified", disk_image: false, containers: ["disk3"], erase_refusal: "disk0 is an internal disk" },
  { device: "disk6", internal: false, bus: "USB", media_name: "Samsung T7", smart: "Verified", disk_image: false, containers: ["disk7"], erase_refusal: null },
];

describe("MacDisks", () => {
  beforeEach(() => stubFetch());
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders nothing when no Mac backend is registered", async () => {
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
    const { container } = renderView(<MacDisks />);
    await flushQueries();
    expect(container).toBeEmptyDOMElement();
  });

  it("offers actions on an external disk and none on the internal one", async () => {
    vi.spyOn(http, "get").mockImplementation(((url: string) =>
      Promise.resolve({
        data: url.startsWith("/backends") ? [{ id: "bkd_macos_studio", name: "studio" }] : url.startsWith("/macos/devices") ? devices : [],
      })) as never);
    renderView(<MacDisks />);
    await flushQueries();
    expect(await screen.findByText("Samsung T7")).toBeTruthy();
    expect(screen.getByText(/refused by Atlas/)).toBeTruthy();
    expect(screen.getAllByText(/Erase…/)).toHaveLength(1);
    expect(screen.getByText(/Resize \(disk7\)…/)).toBeTruthy();
    expect(screen.queryByText(/Resize \(disk3\)…/)).toBeNull();
    expect(http.get).toHaveBeenCalledWith(expect.stringContaining("/macos/devices?backend=bkd_macos_studio"));
  });
});
