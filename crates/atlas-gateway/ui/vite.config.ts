// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// The gateway serves the built app from `/` and the API from `/api/atlas/v1`.
// In dev, proxy API + health/version to a running gateway (default 127.0.0.1:5110).
const GATEWAY = process.env.ATLAS_GATEWAY || "http://127.0.0.1:5110";

export default defineConfig({
  plugins: [react()],
  base: "/",
  build: { outDir: "dist", emptyOutDir: true, chunkSizeWarningLimit: 1500 },
  server: {
    port: 5173,
    proxy: {
      "/api": { target: GATEWAY, changeOrigin: true },
      "/health": { target: GATEWAY, changeOrigin: true },
      "/version": { target: GATEWAY, changeOrigin: true },
    },
  },
  test: {
    // jsdom (not "node") so component tests (`*.test.tsx`) can render — a small, safe overhead
    // for the existing plain-logic `*.test.ts` files, and avoids maintaining two environments.
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
