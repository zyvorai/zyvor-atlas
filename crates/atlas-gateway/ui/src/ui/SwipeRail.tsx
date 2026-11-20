// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import React, { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { ChevronLeft, ChevronRight } from "lucide-react";
import { cx } from "../lib/format";

/** Horizontal scroll-snap gallery (Apple shop feature strip). */
export function SwipeRail({
  children,
  className,
  label = "Gallery",
}: {
  children: ReactNode;
  className?: string;
  label?: string;
}) {
  const trackRef = useRef<HTMLDivElement>(null);
  const [canPrev, setCanPrev] = useState(false);
  const [canNext, setCanNext] = useState(false);

  const sync = useCallback(() => {
    const el = trackRef.current;
    if (!el) return;
    const max = el.scrollWidth - el.clientWidth;
    setCanPrev(el.scrollLeft > 4);
    setCanNext(el.scrollLeft < max - 4);
  }, []);

  useEffect(() => {
    const el = trackRef.current;
    if (!el) return;
    sync();
    el.addEventListener("scroll", sync, { passive: true });
    const ro = new ResizeObserver(sync);
    ro.observe(el);
    return () => {
      el.removeEventListener("scroll", sync);
      ro.disconnect();
    };
  }, [sync, children]);

  const scrollByPage = (dir: -1 | 1) => {
    const el = trackRef.current;
    if (!el) return;
    const amount = Math.max(240, Math.floor(el.clientWidth * 0.85));
    el.scrollBy({ left: dir * amount, behavior: "smooth" });
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowLeft") {
      e.preventDefault();
      scrollByPage(-1);
    } else if (e.key === "ArrowRight") {
      e.preventDefault();
      scrollByPage(1);
    }
  };

  return (
    <div className={cx("at-swipe", className)} role="region" aria-label={label} onKeyDown={onKeyDown}>
      <button
        type="button"
        className="at-swipe-nav prev"
        aria-label="Previous"
        disabled={!canPrev}
        onClick={() => scrollByPage(-1)}
      >
        <ChevronLeft size={18} />
      </button>
      <div className="at-swipe-track" ref={trackRef} tabIndex={0}>
        {React.Children.map(children, (child, i) => (
          <div className="at-swipe-item" key={i}>
            {child}
          </div>
        ))}
      </div>
      <button
        type="button"
        className="at-swipe-nav next"
        aria-label="Next"
        disabled={!canNext}
        onClick={() => scrollByPage(1)}
      >
        <ChevronRight size={18} />
      </button>
    </div>
  );
}

/** Apple shop-style selection tile (config / storage choice). */
export function SelectTile({
  title,
  hint,
  selected,
  onSelect,
  className,
}: {
  title: ReactNode;
  hint?: ReactNode;
  selected?: boolean;
  onSelect?: () => void;
  className?: string;
}) {
  return (
    <button
      type="button"
      className={cx("at-select-tile", selected && "on", className)}
      aria-pressed={!!selected}
      onClick={onSelect}
    >
      <span className="at-select-title">{title}</span>
      {hint != null && hint !== false ? <span className="at-select-hint">{hint}</span> : null}
    </button>
  );
}

export function SelectTileGrid({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cx("at-select-grid", className)}>{children}</div>;
}

export function EmptyBox({
  title,
  children,
  action,
}: {
  title: ReactNode;
  children?: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="at-empty-box">
      <div className="at-empty-title">{title}</div>
      {children}
      {action}
    </div>
  );
}
