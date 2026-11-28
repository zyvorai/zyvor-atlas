// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, screen, within } from "@testing-library/react";
import { flushQueries, renderView, stubFetch } from "../test/renderView";
import { http } from "../api/client";
import Volumes from "./Volumes";

const MiB = 1024 * 1024;
const GiB = 1024 * MiB;

const volumes = [
  { id: "v1", name: "bravo", kind: "block", size_bytes: 2 * GiB, used_bytes: null, state: "bound", health: "ok" },
  { id: "v2", name: "alpha", kind: "block", size_bytes: 3 * GiB, used_bytes: 1.5 * GiB, state: "bound", health: "ok" },
];

function stubVolumes() {
  vi.spyOn(http, "get").mockImplementation(((url: string) =>
    Promise.resolve({ data: url === "/volumes" ? volumes : [] })) as never);
}

/** Volume names in table order. */
const names = () =>
  screen
    .getAllByRole("row")
    .slice(1)
    .map((r) => within(r).getAllByRole("cell")[1].textContent);

describe("Volumes", () => {
  beforeEach(() => {
    stubFetch();
    vi.spyOn(http, "get").mockResolvedValue({ data: [] } as never);
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("renders without a live gateway", async () => {
    const { container } = renderView(<Volumes />);
    await flushQueries();
    expect(container).not.toBeEmptyDOMElement();
  });

  it("offers to create a volume when there are none", async () => {
    renderView(<Volumes />);
    expect(await screen.findByRole("heading", { name: "No volumes yet" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Create volume" }));
    expect(screen.getByRole("dialog", { name: "Create volume" })).toBeInTheDocument();
  });

  it("sorts from real header buttons and says which way", async () => {
    stubVolumes();
    renderView(<Volumes />);
    await screen.findByRole("button", { name: "alpha" });
    expect(names()).toEqual(["alpha", "bravo"]);
    expect(screen.getByRole("columnheader", { name: "Name" })).toHaveAttribute("aria-sort", "ascending");

    // A native button is focusable and fires click on Enter and Space.
    const size = screen.getByRole("button", { name: "Size" });
    expect(size.tagName).toBe("BUTTON");
    fireEvent.click(size);
    expect(names()).toEqual(["bravo", "alpha"]);
    expect(screen.getByRole("columnheader", { name: "Size" })).toHaveAttribute("aria-sort", "ascending");
    expect(screen.getByRole("columnheader", { name: "Name" })).not.toHaveAttribute("aria-sort");
    fireEvent.click(size);
    expect(names()).toEqual(["alpha", "bravo"]);
    expect(screen.getByRole("columnheader", { name: "Size" })).toHaveAttribute("aria-sort", "descending");
    for (const label of ["Name", "Kind", "State"]) {
      expect(screen.getByRole("button", { name: label })).toBeInTheDocument();
    }
  });

  it("closes the filter popover on Escape and hands focus back", async () => {
    renderView(<Volumes />);
    await flushQueries();
    const trigger = screen.getByRole("button", { name: /^Filters/ });
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(trigger);
    const panel = screen.getByRole("dialog", { name: "Filters" });
    const tenant = within(panel).getByPlaceholderText("Any tenant");
    tenant.focus();
    fireEvent.keyDown(tenant, { key: "Escape" });
    expect(screen.queryByRole("dialog", { name: "Filters" })).not.toBeInTheDocument();
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(trigger).toHaveFocus();
  });

  it("keeps the filter popover open for presses inside it and closes it for presses outside", async () => {
    renderView(<Volumes />);
    await flushQueries();
    fireEvent.click(screen.getByRole("button", { name: /^Filters/ }));
    const panel = screen.getByRole("dialog", { name: "Filters" });
    const block = within(panel).getByRole("button", { name: "block" });
    fireEvent.pointerDown(block);
    fireEvent.click(block);
    expect(block).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByRole("button", { name: /^Filters/ })).toHaveTextContent("1");
    fireEvent.pointerDown(document.body);
    expect(screen.queryByRole("dialog", { name: "Filters" })).not.toBeInTheDocument();
  });

  it("draws a usage bar only for usage that was reported", async () => {
    stubVolumes();
    renderView(<Volumes />);
    const alpha = (await screen.findByRole("button", { name: "alpha" })).closest("tr")!;
    const bravo = screen.getByRole("button", { name: "bravo" }).closest("tr")!;
    expect(alpha).toHaveTextContent("1.5 GiB · 50%");
    expect(alpha.querySelector(".at-mini")).not.toBeNull();
    expect(bravo.querySelector(".at-mini")).toBeNull();
    expect(bravo).toHaveTextContent("—");
  });

  it("opens the details panel from the volume name", async () => {
    stubVolumes();
    renderView(<Volumes />);
    fireEvent.click(await screen.findByRole("button", { name: "bravo" }));
    const drawer = screen.getByRole("dialog", { name: "bravo" });
    expect(
      within(drawer).getByRole("img", { name: /Volume usage: 2 GiB total · usage not reported/ }),
    ).toBeInTheDocument();
  });

  it("asks before deleting and leaves the row alone until then", async () => {
    stubVolumes();
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(false);
    const request = vi.spyOn(http, "request");
    const remove = vi.spyOn(http, "delete");
    renderView(<Volumes />);
    const alpha = (await screen.findByRole("button", { name: "alpha" })).closest("tr")!;
    fireEvent.click(within(alpha).getByRole("button", { name: "Delete" }));
    await flushQueries();
    expect(confirmSpy).toHaveBeenCalledWith("This action cannot be undone.");
    expect(request).not.toHaveBeenCalled();
    expect(remove).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog", { name: "alpha" })).not.toBeInTheDocument();
  });
});
