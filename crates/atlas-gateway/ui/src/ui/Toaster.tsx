// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { useEffect, useState } from "react";
import { CheckCircle2, Info, XCircle } from "lucide-react";
import { onClearToasts, onToast } from "../api/client";

type T = { id: number; kind: "info" | "ok" | "err"; msg: string };

export function Toaster() {
  const [items, setItems] = useState<T[]>([]);
  useEffect(() => {
    const offToast = onToast((t) => {
      setItems((s) => [...s, t]);
      setTimeout(() => setItems((s) => s.filter((x) => x.id !== t.id)), 4500);
    });
    const offClear = onClearToasts(() => setItems([]));
    return () => {
      offToast();
      offClear();
    };
  }, []);
  return (
    // Clears page edge.
    <div className="fixed right-4 bottom-4 z-60 flex flex-col gap-2 w-[300px]">
      {items.map((t) => (
        <div
          key={t.id}
          className="at-glass px-3.5 py-2.5 flex items-center gap-2.5 text-sm animate-fade-in"
          style={{
            borderRadius: 14,
            borderLeft: `3px solid ${t.kind === "ok" ? "#30D69E" : t.kind === "err" ? "#E23B3B" : "#38BDF8"}`,
          }}
        >
          {t.kind === "ok" ? (
            <CheckCircle2 size={16} className="text-success shrink-0" />
          ) : t.kind === "err" ? (
            <XCircle size={16} className="text-danger shrink-0" />
          ) : (
            <Info size={16} className="text-sky-400 shrink-0" />
          )}
          <span className="min-w-0">{t.msg}</span>
        </div>
      ))}
    </div>
  );
}
