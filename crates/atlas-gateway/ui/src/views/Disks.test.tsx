// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { fireEvent, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import Disks from "./Disks";

const TiB = 1024 ** 4;
const zpool = { id: "z1", cluster_id: "c", name: "tank0", kind: "zpool", used_bytes: TiB, max_bytes: 4 * TiB, health: "ok" };

describe("Disks", () => {
  beforeEach(() => {
    stubFetch();
    vi.spyOn(http, "get").mockImplementation(((url: string) =>
      Promise.resolve({ data: url === "/pools" ? [zpool] : [] })) as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("draws a zpool's fill from its total size", async () => {
    renderView(<Disks />);
    const list = await screen.findByRole("list", { name: "ZFS pool capacity" });
    expect(within(list).getByText("25%")).toBeInTheDocument();
  });

  it("destroys a pool only after its name is typed back", async () => {
    const request = vi.spyOn(http, "request").mockResolvedValue({ data: {} } as never);
    renderView(<Disks />);
    fireEvent.click(await screen.findByRole("button", { name: "Destroy…" }));
    const dialog = screen.getByRole("dialog");
    const submit = within(dialog).getByRole("button", { name: "Destroy pool (irreversible)" });
    expect(submit).toBeDisabled();
    const input = within(dialog).getByLabelText('Type "tank0" to confirm');
    fireEvent.change(input, { target: { value: "tank" } });
    expect(submit).toBeDisabled();
    expect(request).not.toHaveBeenCalled();
    fireEvent.change(input, { target: { value: "tank0" } });
    expect(submit).toBeEnabled();
    fireEvent.click(submit);
    await flushQueries();
    expect(request).toHaveBeenCalledWith({
      method: "post",
      url: "/zfs/pools/tank0/destroy",
      data: { confirm_pool_name: "tank0" },
    });
  });

  it("keeps disk provisioning disabled until the device path is typed back", async () => {
    renderView(<Disks />);
    await flushQueries();
    fireEvent.click(screen.getByRole("button", { name: "Provision device…" }));
    expect(within(screen.getByRole("dialog")).getByRole("button", { name: "Provision (destructive)" })).toBeDisabled();
  });
});
