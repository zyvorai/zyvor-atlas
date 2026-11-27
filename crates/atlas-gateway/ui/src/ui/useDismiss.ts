// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import { useEffect, useRef } from "react";

/**
 * Closes a popover on Escape or a pointer press outside it. Attach the returned ref to the element
 * wrapping both the trigger and the panel; Escape hands focus back to the open trigger.
 */
export function useDismiss<T extends HTMLElement>(open: boolean, onClose: () => void) {
  const ref = useRef<T>(null);
  const close = useRef(onClose);
  useEffect(() => {
    close.current = onClose;
  });
  useEffect(() => {
    if (!open) return;
    const onPointer = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) close.current();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      const root = ref.current;
      if (root?.contains(document.activeElement)) {
        (root.querySelector<HTMLElement>('button[aria-expanded="true"]') ?? root.querySelector("button"))?.focus();
      }
      close.current();
    };
    document.addEventListener("pointerdown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);
  return ref;
}
