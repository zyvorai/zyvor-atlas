// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
// Re-exports from routes.ts for backward compatibility.
export type { NavModule as Module, SectionId } from "./routes";
export {
  MODULES,
  SECTIONS,
  SECTION_META,
  SPARK,
  modulesForRole,
  canModuleAccess,
  pinnedModules,
  activeModuleFromPath,
  navLabelForPath,
  sectionForPath,
  APP_ROUTES,
  shortcutTargets,
  moduleById,
  navCrumbs,
} from "./routes";
