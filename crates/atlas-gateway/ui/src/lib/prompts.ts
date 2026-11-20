// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
/** Agent handoff stub — reasoning-only CTAs; deterministic nav stays on the router. */

type PromptListener = (q: string) => void;
const listeners = new Set<PromptListener>();

export function onSendPrompt(fn: PromptListener): () => void {
  listeners.add(fn);
  return () => {
    listeners.delete(fn);
  };
}

export function sendPrompt(q: string) {
  const text = q.trim();
  if (!text) return;
  listeners.forEach((fn) => fn(text));
  if (typeof window !== "undefined") {
    (window as Window & { sendPrompt?: (q: string) => void }).sendPrompt = sendPrompt;
  }
}

declare global {
  interface Window {
    sendPrompt?: (q: string) => void;
  }
}

if (typeof window !== "undefined") {
  window.sendPrompt = sendPrompt;
}
