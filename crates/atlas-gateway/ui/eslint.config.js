// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import js from "@eslint/js";
import tseslint from "typescript-eslint";
import reactHooks from "eslint-plugin-react-hooks";
import globals from "globals";

// Flat config (this is a newer Vite/React setup than zeus-os's legacy .eslintrc.cjs) —
// mirrors zeus-os's rule *selection* (recommended JS + TS + react-hooks), not its config format.
export default tseslint.config(
  { ignores: ["dist", "node_modules"] },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    files: ["**/*.{ts,tsx}"],
    languageOptions: {
      ecmaVersion: 2022,
      globals: { ...globals.browser, ...globals.node },
    },
    plugins: { "react-hooks": reactHooks },
    rules: {
      ...reactHooks.configs.recommended.rules,
      // Same convention as zeus-os: unused vars/args prefixed with _ are intentional.
      "@typescript-eslint/no-unused-vars": [
        "warn",
        { argsIgnorePattern: "^_", varsIgnorePattern: "^_" },
      ],
      // First-pass lint adoption on an existing, working codebase: `any` shows up ~55 times
      // across API glue/legacy views. Turning this into a hard gate in the same pass that
      // *introduces* lint would mean either a large, risky rewrite or an equally large
      // suppression-comment sweep, neither of which is this pass's goal (catching real bugs:
      // rules-of-hooks, unused-expressions, exhaustive-deps). Left off deliberately — tighten
      // per-directory later, the way zeus-os's own .eslintrc.cjs does for its newer views.
      "@typescript-eslint/no-explicit-any": "off",
      // `cond && fn()` (fire-and-forget cleanup calls) is a safe, common idiom here — don't
      // flag it alongside the genuinely-suspicious "ternary used only for its side effect"
      // shape, which stays an error.
      "@typescript-eslint/no-unused-expressions": ["error", { allowShortCircuit: true }],
    },
  },
);
