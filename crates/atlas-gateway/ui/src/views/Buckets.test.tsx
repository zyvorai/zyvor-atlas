// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, screen, waitFor } from "@testing-library/react";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import type { StorageBucket } from "../api/types";
import Buckets from "./Buckets";

const row = (id: string, name: string, extra: Partial<StorageBucket> = {}): StorageBucket => ({
  id,
  tenant_id: "global",
  name,
  bucket_name: name,
  backend_id: "bkd_ceph_lab",
  state: "bound",
  ...extra,
});

describe("Buckets", () => {
  beforeEach(() => {
    stubFetch();
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders without a live gateway", async () => {
    const { container } = renderView(<Buckets />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });

  const withRows = (rows: StorageBucket[]) =>
    vi.spyOn(http, "get").mockImplementation(((url: string) =>
      Promise.resolve({ data: url.startsWith("/buckets") ? rows : [] })) as never);

  it("offers Settings only where the gateway can serve it", async () => {
    withRows([
      row("bkt_1", "photos"),
      row("bkt_2", "waiting", { state: "pending" }),
      row("bkt_3", "legacy", { backend_id: "bkd_rustfs_lab" }),
    ]);
    renderView(<Buckets />);
    await screen.findByText("photos");
    const buttons = screen.getAllByRole("button", { name: "Settings" });
    expect(buttons).toHaveLength(1);
    expect(buttons[0].closest("tr")).toHaveTextContent("photos");
  });

  it("opens the settings panel, keeps keyboard focus in it, and hands it back on Escape", async () => {
    withRows([row("bkt_1", "photos")]);
    const request = vi.spyOn(http, "request").mockResolvedValue({ status: 200, data: "" } as never);
    renderView(<Buckets />);
    const trigger = await screen.findByRole("button", { name: "Settings" });
    trigger.focus();
    fireEvent.click(trigger);

    const dialog = screen.getByRole("dialog", { name: "photos · settings" });
    expect(dialog).toHaveFocus();
    await waitFor(() =>
      expect(request).toHaveBeenCalledWith(
        expect.objectContaining({ method: "GET", url: "/buckets/bkt_1/subresource?versioning" }),
      ),
    );
    await screen.findByText("Unversioned");

    const enabled = Array.from(dialog.querySelectorAll<HTMLElement>("button:not([disabled])"));
    const first = enabled[0];
    const last = enabled[enabled.length - 1];
    fireEvent.keyDown(dialog, { key: "Tab", shiftKey: true });
    expect(last).toHaveFocus();
    fireEvent.keyDown(last, { key: "Tab" });
    expect(first).toHaveFocus();

    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(trigger).toHaveFocus();
  });
});
