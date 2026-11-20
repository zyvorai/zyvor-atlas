// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";
import "@testing-library/jest-dom/vitest";

// @testing-library/react's own auto-cleanup only registers itself when it finds a global
// `afterEach` (i.e. vitest's `test.globals: true`), which this project's vite.config.ts doesn't
// set — so without this, a render from one test stays mounted into the next `it` in the same
// file, and any document-wide query (screen.getByLabelText, etc.) can silently match both.
afterEach(() => cleanup());

// jsdom doesn't implement scroll APIs; several views call them from effects on mount
// (chapter navigation, scroll-to-top) purely for UX, so a no-op stub is enough to render.
Element.prototype.scrollTo = Element.prototype.scrollTo || (() => {});
Element.prototype.scrollIntoView = Element.prototype.scrollIntoView || (() => {});
window.scrollTo = window.scrollTo || (() => {});

// Chart libraries (recharts) measure their container via ResizeObserver, which jsdom lacks.
class ResizeObserverStub implements ResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
}
window.ResizeObserver = window.ResizeObserver || ResizeObserverStub;
