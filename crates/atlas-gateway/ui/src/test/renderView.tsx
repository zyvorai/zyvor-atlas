// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Shared smoke-test harness: mount a view inside the providers it needs at runtime
// (react-query + router) with the axios client and global fetch stubbed to resolve
// empty/ok responses, so every view can be smoke-tested without a live gateway.
import { act, render } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import type { ReactElement } from "react";
import { vi } from "vitest";

/** A query client with retries off so a smoke test doesn't hang or slow down on 404s. */
export function newTestQueryClient() {
  return new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
}

/**
 * Renders `ui` wrapped in the router + react-query providers every view assumes it has when
 * mounted inside the app shell. Uses a fresh QueryClient per call so query state never leaks
 * between tests.
 */
export function renderView(ui: ReactElement, route = "/") {
  const client = newTestQueryClient();
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={[route]}>{ui}</MemoryRouter>
    </QueryClientProvider>,
  );
}

/**
 * Flushes the microtask queue inside `act()` so the mocked query responses resolve and any
 * resulting re-render happens — and, critically, re-throws inside the test (instead of surfacing
 * as a separate "unhandled rejection") if that re-render throws. Await this after `renderView`
 * before asserting, since react-query resolves asynchronously even with an already-mocked client.
 */
export async function flushQueries() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

/** Stubs `window.fetch` (used directly by a few views alongside the axios client) to a benign 200/{}. */
export function stubFetch() {
  vi.stubGlobal(
    "fetch",
    vi.fn(() =>
      Promise.resolve({
        ok: true,
        status: 200,
        json: () => Promise.resolve({}),
      } as Response),
    ),
  );
}
