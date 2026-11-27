// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
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
    <div className="at-toaster" role="status" aria-live="polite">
      {items.map((t) => (
        <div key={t.id} className={`at-glass at-toast-item ${t.kind}`}>
          {t.kind === "ok" ? (
            <CheckCircle2 size={17} aria-hidden />
          ) : t.kind === "err" ? (
            <XCircle size={17} aria-hidden />
          ) : (
            <Info size={17} aria-hidden />
          )}
          <span className="min-w-0 break-words">{t.msg}</span>
        </div>
      ))}
    </div>
  );
}
