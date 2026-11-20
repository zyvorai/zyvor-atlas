// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Imperative confirm() dialog — `await confirm({...})` from anywhere; a single <ConfirmHost/> renders it.
import { useEffect, useRef, useState } from "react";
import { AlertTriangle } from "lucide-react";
import { Button, Modal } from "./kit";

export interface ConfirmOpts {
  title?: string;
  message: string;
  confirmLabel?: string;
  cancelLabel?: string;
  danger?: boolean;
}

let opener: ((o: ConfirmOpts) => Promise<boolean>) | null = null;

export function confirm(o: ConfirmOpts): Promise<boolean> {
  return opener ? opener(o) : Promise.resolve(window.confirm(o.message));
}

/** Confirm a destructive action, then run it. `run` is typically submit()/submitJob(), which already
 * toasts on failure and rejects; that rejection has no other listener here, so without this catch
 * every failed delete/action throws an unhandled promise rejection. */
export async function confirmThen(o: ConfirmOpts, run: () => void) {
  if (await confirm(o)) Promise.resolve(run()).catch(() => {});
}
export const del = (what: string, run: () => void) =>
  confirmThen(
    { title: `Delete ${what}?`, message: "This action cannot be undone.", confirmLabel: "Delete", danger: true },
    run,
  );

export function ConfirmHost() {
  const [state, setState] = useState<{ o: ConfirmOpts; resolve: (v: boolean) => void } | null>(null);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const confirmRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    opener = (o) => new Promise<boolean>((resolve) => setState({ o, resolve }));
    return () => {
      opener = null;
    };
  }, []);

  useEffect(() => {
    if (!state) return;
    // Safer default: focus Cancel on destructive confirms; Confirm otherwise.
    const t = window.setTimeout(() => {
      (state.o.danger ? cancelRef.current : confirmRef.current)?.focus();
    }, 0);
    return () => window.clearTimeout(t);
  }, [state]);

  if (!state) return null;
  const done = (v: boolean) => {
    state.resolve(v);
    setState(null);
  };
  const danger = !!state.o.danger;
  return (
    <Modal
      open
      onClose={() => done(false)}
      title={
        <span className="flex items-center gap-2">
          <AlertTriangle size={16} className={danger ? "text-danger" : "text-warning"} />
          {state.o.title || "Confirm"}
        </span>
      }
      footer={
        <>
          <Button ref={cancelRef} onClick={() => done(false)}>
            {state.o.cancelLabel || "Cancel"}
          </Button>
          <Button ref={confirmRef} variant={danger ? "danger" : "primary"} onClick={() => done(true)}>
            {state.o.confirmLabel || "Confirm"}
          </Button>
        </>
      }
    >
      <div className="text-sm text-muted-foreground">{state.o.message}</div>
    </Modal>
  );
}
