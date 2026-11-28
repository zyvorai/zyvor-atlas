// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
import React, { useEffect, useMemo, useState } from "react";
import { AlertTriangle, ChevronDown, ChevronUp } from "lucide-react";
import { Button, EmptyState, Spinner } from "./kit";

export type Col<T> = {
  h: string;
  f: (r: T) => React.ReactNode;
  mono?: boolean;
  sortKey?: (r: T) => string | number;
};

export function Table<T>({
  cols,
  rows,
  actions,
  onRow,
  empty = "Nothing here yet.",
  emptyCta,
  rowKey,
  selectable,
  selected,
  onToggle,
  onToggleAll,
  error,
  errorDetail,
  onRetry,
  soundings,
  panelTitle,
  panelExtra,
}: {
  cols: Col<T>[];
  rows: T[] | undefined;
  actions?: (r: T) => React.ReactNode;
  onRow?: (r: T) => void;
  empty?: string;
  emptyCta?: React.ReactNode;
  rowKey?: (r: T, i: number) => string;
  selectable?: boolean;
  selected?: Set<string>;
  onToggle?: (key: string) => void;
  onToggleAll?: (keys: string[]) => void;
  error?: boolean;
  /** Optional gateway/network detail under the generic failed-to-load line. */
  errorDetail?: string;
  onRetry?: () => void;
  /** Index layout — the table sits in a panel with an optional title bar. */
  soundings?: boolean;
  panelTitle?: React.ReactNode;
  panelExtra?: React.ReactNode;
}) {
  const [sort, setSort] = useState<{ i: number; dir: 1 | -1 } | null>(null);
  const sorted = useMemo(() => {
    if (!rows || !sort) return rows;
    const key = cols[sort.i]?.sortKey;
    if (!key) return rows;
    return [...rows].sort((a, b) => {
      const ka = key(a);
      const kb = key(b);
      return (ka < kb ? -1 : ka > kb ? 1 : 0) * sort.dir;
    });
  }, [rows, sort, cols]);

  const [stuck, setStuck] = useState(false);
  useEffect(() => {
    if (rows) {
      // Data arrived — clear the "stuck loading" flag armed by the timeout below.
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setStuck(false);
      return;
    }
    const t = setTimeout(() => setStuck(true), 10000);
    return () => clearTimeout(t);
  }, [rows]);

  const body = (() => {
    if (!rows) {
      if (error || stuck) {
        return (
          <div className="at-table-error" role="alert">
            <AlertTriangle size={18} aria-hidden />
            <span>Failed to load — the request errored.</span>
            {errorDetail ? <span className="detail mono">{errorDetail}</span> : null}
            {onRetry ? (
              <Button size="sm" onClick={onRetry} className="mt-1">
                Retry
              </Button>
            ) : (
              <span>Try refreshing.</span>
            )}
          </div>
        );
      }
      return soundings ? <div className="at-loading">Loading…</div> : <Spinner />;
    }
    if (!rows.length) {
      return (
        <div style={{ margin: soundings ? 16 : 0 }}>
          <EmptyState msg={empty} cta={emptyCta} />
        </div>
      );
    }

    const clickHeader = (i: number) => {
      if (!cols[i].sortKey) return;
      setSort((s) => (s?.i === i ? { i, dir: s.dir === 1 ? -1 : 1 } : { i, dir: 1 }));
    };
    const rk = (r: T, i: number) => (rowKey ? rowKey(r, i) : String(i));
    const allKeys = (sorted || []).map((r, i) => rk(r, i));
    const allSelected = selectable && allKeys.length > 0 && allKeys.every((k) => selected?.has(k));

    return (
      // A wide table scrolls sideways on narrow screens, so the scroller must take keyboard focus.
      // `relative` keeps the visually hidden "Actions" heading inside the scroller; without it the
      // absolutely positioned span widens whatever holds the table.
      <div
        className="relative overflow-x-auto"
        role="region"
        tabIndex={0}
        aria-label={typeof panelTitle === "string" ? `${panelTitle} table` : "Data table"}
      >
        <table className={soundings ? "at-tbl" : "ztable"}>
          <thead>
            <tr>
              {selectable && (
                <th style={{ width: 34 }}>
                  <input
                    type="checkbox"
                    checked={!!allSelected}
                    onChange={() => onToggleAll?.(allKeys)}
                    aria-label="Select all"
                  />
                </th>
              )}
              {cols.map((c, i) => (
                <th
                  key={c.h}
                  onClick={() => clickHeader(i)}
                  className={c.sortKey ? "sortable" : undefined}
                  aria-sort={sort?.i === i ? (sort.dir === 1 ? "ascending" : "descending") : undefined}
                >
                  {c.sortKey ? (
                    // The click bubbles to the header cell, which does the sorting.
                    <button type="button" className="at-th-sort">
                      {c.h}
                      {sort?.i === i &&
                        (sort.dir === 1 ? <ChevronUp size={12} aria-hidden /> : <ChevronDown size={12} aria-hidden />)}
                    </button>
                  ) : (
                    c.h
                  )}
                </th>
              ))}
              {actions && (
                <th>
                  <span className="sr-only">Actions</span>
                </th>
              )}
            </tr>
          </thead>
          <tbody>
            {(sorted || []).map((r, i) => {
              const key = rk(r, i);
              return (
                <tr
                  key={key}
                  onClick={onRow ? () => onRow(r) : undefined}
                  tabIndex={onRow ? 0 : undefined}
                  onKeyDown={
                    onRow
                      ? (e) => {
                          // Only the row itself: controls inside it keep their own Enter handling.
                          if (e.key === "Enter" && e.target === e.currentTarget) onRow(r);
                        }
                      : undefined
                  }
                  className={`${onRow ? "cursor-pointer" : ""} ${selected?.has(key) ? "selected" : ""}`}
                >
                  {selectable && (
                    <td onClick={(e) => e.stopPropagation()}>
                      <input
                        type="checkbox"
                        checked={selected?.has(key) || false}
                        onChange={() => onToggle?.(key)}
                        aria-label={`Select ${key}`}
                      />
                    </td>
                  )}
                  {cols.map((c) => (
                    <td key={c.h} className={c.mono ? "mono" : undefined}>
                      {c.f(r) ?? "—"}
                    </td>
                  ))}
                  {actions && (
                    <td onClick={(e) => e.stopPropagation()}>
                      <div className={soundings ? "at-row-actions" : "flex gap-1.5 justify-end"}>{actions(r)}</div>
                    </td>
                  )}
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    );
  })();

  if (!soundings) return body;

  return (
    <div className="at-panel">
      {(panelTitle != null || panelExtra != null) && (
        <div className="at-panel-bar">
          {panelTitle != null && <span className="at-caption">{panelTitle}</span>}
          <span className="grow" />
          {panelExtra}
          {rows && rows.length > 0 && panelExtra == null && (
            <span className="at-sub" style={{ margin: 0 }}>
              {rows.length} shown
            </span>
          )}
        </div>
      )}
      {body}
    </div>
  );
}
