// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { useEffect, useState } from "react";

/**
 * Tracks an element's content width so an SVG can be laid out at 1:1 pixel size (text stays
 * legible on a phone instead of shrinking with the viewBox). Returns a callback ref, so it
 * works when the element appears only after data arrives.
 */
export function useElementWidth(fallback: number): [(el: Element | null) => void, number] {
  const [el, setEl] = useState<Element | null>(null);
  const [width, setWidth] = useState(fallback);

  useEffect(() => {
    if (!el || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver((entries) => {
      const w = entries[0]?.contentRect.width;
      if (w && w > 0) setWidth(Math.round(w));
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, [el]);

  return [setEl, width];
}
