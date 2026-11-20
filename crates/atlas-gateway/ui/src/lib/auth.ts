// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
/** Client-side JWT role decode — server enforces; this is for nav UX only. */

export type NavRole = "viewer" | "operator" | "admin";

export function roleLabel(role: NavRole): string {
  return role.charAt(0).toUpperCase() + role.slice(1);
}

export const ROLE_VIEWER = 0;
export const ROLE_OPERATOR = 1;
export const ROLE_ADMIN = 2;

/** Match server `role_level` in atlas-gateway auth.rs */
export function roleLevel(role: string): number {
  switch (role) {
    case "admin":
    case "storage.admin":
    case "storage.security":
    case "storage.breakglass":
      return ROLE_ADMIN;
    case "operator":
    case "storage.operator":
      return ROLE_OPERATOR;
    default:
      if (role.startsWith("product.service.")) return ROLE_OPERATOR;
      return ROLE_VIEWER;
  }
}

export function normalizeNavRole(role: string): NavRole {
  const level = roleLevel(role);
  if (level >= ROLE_ADMIN) return "admin";
  if (level >= ROLE_OPERATOR) return "operator";
  return "viewer";
}

export function minRoleLevel(minRole: NavRole): number {
  switch (minRole) {
    case "admin":
      return ROLE_ADMIN;
    case "operator":
      return ROLE_OPERATOR;
    default:
      return ROLE_VIEWER;
  }
}

export function canAccess(minRole: NavRole | undefined, actorLevel: number): boolean {
  return actorLevel >= minRoleLevel(minRole ?? "viewer");
}

/** Decode JWT payload without verification (nav gating only). */
export function roleFromToken(token: string): NavRole {
  if (!token) return "viewer";
  const parts = token.split(".");
  if (parts.length < 2) return "viewer";
  try {
    const json = atob(parts[1].replace(/-/g, "+").replace(/_/g, "/"));
    const payload = JSON.parse(json) as { role?: string };
    return normalizeNavRole(payload.role ?? "viewer");
  } catch {
    return "viewer";
  }
}
