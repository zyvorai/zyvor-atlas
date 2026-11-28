// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { screen } from "@testing-library/react";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import Rbd from "./Rbd";

const GiB = 1024 * 1024 * 1024;

describe("Rbd", () => {
  beforeEach(() => {
    stubFetch();
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders without a live gateway", async () => {
    const { container } = renderView(<Rbd />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });

  it("shows usage only for images the volume inventory measures", async () => {
    vi.spyOn(http, "get").mockImplementation(((url: string) =>
      Promise.resolve({
        data: url.startsWith("/rbd-images")
          ? { pool: "rbd-nvme-prod", images: ["tracked", "untracked"] }
          : url === "/volumes"
            ? [{ id: "v1", name: "v1", backend_native_id: "rbd:rbd-nvme-prod/tracked", size_bytes: 4 * GiB, used_bytes: GiB }]
            : [],
      })) as never);
    renderView(<Rbd />);
    expect(await screen.findByText("1.0 GiB · 25%")).toBeInTheDocument();
    const untracked = screen.getByText("untracked").closest("tr")!;
    expect(untracked.querySelector(".at-mini")).toBeNull();
  });

  it("offers to create an image in an empty pool", async () => {
    vi.spyOn(http, "get").mockImplementation(((url: string) =>
      Promise.resolve({ data: url.startsWith("/rbd-images") ? { pool: "rbd-nvme-prod", images: [] } : [] })) as never);
    renderView(<Rbd />);
    expect(await screen.findByRole("heading", { name: "No RBD images in this pool." })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create image" })).toBeInTheDocument();
  });
});
