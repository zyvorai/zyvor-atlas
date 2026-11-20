#!/usr/bin/env node
// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
import { mkdirSync, readFileSync, writeFileSync, existsSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../..')
const PAGES = resolve(ROOT, 'docs/customer/pages')
const { routes } = JSON.parse(readFileSync(resolve(ROOT, 'scripts/customer-docs/routes.json'), 'utf8'))
const purposes = JSON.parse(readFileSync(resolve(ROOT, 'scripts/customer-docs/page-purposes.json'), 'utf8'))

function catDir(category) {
  return category
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-|-$/g, '') || 'other'
}

function slug(path) {
  return path.replace(/^\//, '').replace(/\//g, '-').replace(/:/g, '') || 'home'
}

function guideTemplate({ title, path, category, purpose }) {
  return `# ${title}

## Purpose

${purpose}

## When to use it

- Open this page when the job matches the purpose above
- Prefer **Command Deck** (\`/\`) first if you are unsure where to start
- Confirm gateway auth and backend connectivity if inventories look empty

## How to get there

- Route: \`${path}\`
- Nav: **${category} → ${title}** (sidebar, dock, or spotlight)

## What you can do

1. Open \`${path}\` and wait for live data from the Atlas gateway (default **:5110**).
2. Use filters (backend, tenant, kind, status) when the page provides them.
3. Drill into a volume, job, or plan for detail — mutations return durable jobs (\`202\` + job id).
4. For mutating actions (provision, backup, migrate, DR): review tenant quotas and job status in **Jobs**.

If the page stays empty, check \`/health\`, auth (\`ATLAS_AUTH_REQUIRED\` / JWT), that a storage driver is registered, and run \`atlasctl discover\` if inventory is cold.

## Related pages

- [Getting Started](../../getting-started.md)
- [Command Deck](../storage/home.md)
- [Volumes](../storage/volumes.md)
- [Page index](../../PAGE_INDEX.md)
`
}

let written = 0
let skipped = 0
for (const r of routes) {
  const dir = catDir(r.category)
  const file = join(PAGES, dir, `${slug(r.path)}.md`)
  mkdirSync(dirname(file), { recursive: true })
  if (existsSync(file)) {
    skipped++
    continue
  }
  writeFileSync(
    file,
    guideTemplate({
      title: r.label,
      path: r.path,
      category: r.category,
      purpose: purposes[r.path] || `${r.label} page.`,
    }),
  )
  written++
}
console.log(`Wrote ${written} guides (skipped existing ${skipped})`)
