#!/usr/bin/env node
// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
/**
 * Atlas — client demo reel (website + 1080p). Same pipeline as the Zeus OS wow reel
 * (zeus-os/ui/scripts/record-wow-reel.mjs): Playwright drives a real Chromium session with a
 * burned-in caption/subtitle bar, `recordVideo` captures raw webm, ffmpeg encodes the final mp4s.
 *
 * Requires `playwright` (with Chromium installed) resolvable from this file's directory —
 * `npm i -D playwright && npx playwright install chromium` in a workspace above this script,
 * or run from a checkout that already has it (e.g. symlink node_modules from zeus-os/ui).
 *
 * Usage:
 *   node scripts/demo/record-atlas-demo.mjs http://<gateway-host>:<port>
 *
 * Env overrides: ATLAS_USER, ATLAS_PASSWORD (defaults: admin / Admin@321 — the deploy-remote.sh
 * bootstrap credentials, see deploy/k8s/atlas-gateway.yaml).
 */
import { mkdirSync, readdirSync, unlinkSync } from 'node:fs'
import { join } from 'node:path'
import { spawnSync } from 'node:child_process'
import { chromium } from 'playwright'

const BASE = (process.argv.find((a) => a.startsWith('http')) || 'http://127.0.0.1:5110').replace(/\/$/, '')
const USER = process.env.ATLAS_USER || 'admin'
const PASS = process.env.ATLAS_PASSWORD || 'Admin@321'
const OUT = process.env.ATLAS_DEMO_OUT || '/tmp/atlas-wow-reel'
const RAW = join(OUT, 'raw')
const SHOTS = join(OUT, 'shots')
const FINAL_WEB = join(OUT, 'out', 'atlas-storage-center-demo.mp4')
const FINAL_HD = join(OUT, 'out', 'atlas-storage-center-demo-1080p.mp4')

mkdirSync(RAW, { recursive: true })
mkdirSync(SHOTS, { recursive: true })
mkdirSync(join(OUT, 'out'), { recursive: true })

async function ensureCaptionCss(page) {
  await page
    .addStyleTag({
      content: `
      #atlas-demo-caption {
        position: fixed !important; left: 50% !important; bottom: 32px !important;
        transform: translateX(-50%) !important; z-index: 2147483647 !important;
        max-width: min(980px, 92vw) !important; padding: 16px 24px !important;
        border-radius: 16px !important; background: rgba(6, 10, 18, 0.9) !important;
        color: #f4f7fb !important; font-family: "IBM Plex Sans", "Segoe UI", system-ui, sans-serif !important;
        font-size: 22px !important; font-weight: 560 !important; letter-spacing: 0.01em !important;
        line-height: 1.35 !important; text-align: center !important;
        box-shadow: 0 16px 48px rgba(0,0,0,0.5) !important;
        border: 1px solid rgba(255,255,255,0.16) !important;
        pointer-events: none !important; backdrop-filter: blur(12px) !important;
      }
      #atlas-demo-caption-brand {
        font-size: 11px !important; letter-spacing: 0.16em !important;
        text-transform: uppercase !important; color: rgba(140, 190, 255, 0.95) !important;
        margin-bottom: 6px !important; font-weight: 700 !important;
      }
    `,
    })
    .catch(() => {})
}

async function caption(page, text, holdMs = 2600) {
  await ensureCaptionCss(page)
  await page.evaluate((msg) => {
    let bar = document.getElementById('atlas-demo-caption')
    if (!bar) {
      bar = document.createElement('div')
      bar.id = 'atlas-demo-caption'
      bar.innerHTML =
        '<div id="atlas-demo-caption-brand">Atlas &middot; Storage Center Demo</div><div id="atlas-demo-caption-text"></div>'
      document.documentElement.appendChild(bar)
    }
    const body = document.getElementById('atlas-demo-caption-text')
    if (body) body.textContent = msg
  }, text)
  console.log(`[caption] ${text}`)
  await page.waitForTimeout(holdMs)
}

async function dismissNoise(page) {
  for (const name of ['Dismiss', 'Skip', 'Got it', 'Close tour', 'Not now', 'Close']) {
    const b = page.getByRole('button', { name: new RegExp(`^${name}$`, 'i') }).first()
    if (await b.isVisible().catch(() => false)) await b.click().catch(() => {})
  }
  await page.keyboard.press('Escape').catch(() => {})
  await page.waitForTimeout(200)
}

async function gotoPath(page, path) {
  await page.goto(`${BASE}${path}`, { waitUntil: 'domcontentloaded', timeout: 60000 })
  await page.waitForTimeout(1300)
  await dismissNoise(page)
  await ensureCaptionCss(page)
}

function encode(srcWebm, dest, w, h) {
  const ff = spawnSync(
    'ffmpeg',
    [
      '-y',
      '-i',
      srcWebm,
      '-vf',
      `scale=${w}:${h}:force_original_aspect_ratio=decrease,pad=${w}:${h}:(ow-iw)/2:(oh-ih)/2,format=yuv420p`,
      '-c:v',
      'libx264',
      '-preset',
      'medium',
      '-crf',
      '18',
      '-movflags',
      '+faststart',
      '-an',
      dest,
    ],
    { encoding: 'utf8' },
  )
  if (ff.status !== 0) {
    console.error(ff.stderr?.slice(-1500))
    throw new Error(`ffmpeg failed → ${dest}`)
  }
  return dest
}

async function main() {
  console.log(`[atlas-demo] base=${BASE}`)

  for (const f of readdirSync(RAW)) {
    try {
      unlinkSync(join(RAW, f))
    } catch {
      /* ignore */
    }
  }

  const browser = await chromium.launch()
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 1440, height: 900 },
    recordVideo: { dir: RAW, size: { width: 1440, height: 900 } },
  })
  const page = await context.newPage()
  const t0 = Date.now()

  // —— Act 1: Sign in ——
  await page.goto(BASE, { waitUntil: 'domcontentloaded', timeout: 60000 })
  await page.waitForTimeout(1200)
  await ensureCaptionCss(page)
  await caption(page, 'Atlas — the storage control plane for the Zyvor product suite.', 2800)
  await page.fill('#atlas-username', USER)
  await page.fill('#atlas-password', PASS)
  await caption(page, 'Signing in as an operator…', 1400)
  await page.click('button:has-text("Sign in to Atlas")')
  await page.waitForTimeout(2000)
  await dismissNoise(page)
  await ensureCaptionCss(page)
  await page.screenshot({ path: join(SHOTS, '01-login.png') })

  // —— Act 2: Command Deck ——
  await caption(page, 'Command Deck — surveyed capacity, live throughput, everything at a glance.', 3200)
  await page.screenshot({ path: join(SHOTS, '02-command-deck.png') })
  await page.mouse.wheel(0, 480)
  await page.waitForTimeout(400)
  await caption(page, 'Pool soundings, OSD health, and the cluster ledger — one page, no wall of widgets.', 3200)
  await page.screenshot({ path: join(SHOTS, '03-pools.png') })
  await page.mouse.wheel(0, -480)
  await page.waitForTimeout(600)

  // —— Act 3: Volumes ——
  await gotoPath(page, '/volumes')
  await caption(page, 'Volumes — block and filesystem, PVC-backed, at a glance.', 2800)
  await page.screenshot({ path: join(SHOTS, '04-volumes.png') })
  const row = page.getByText('billing-db-01-root').first()
  if (await row.isVisible().catch(() => false)) {
    await row.click().catch(() => {})
    await page.waitForTimeout(900)
    await caption(page, 'Every volume: ID, class, PVC binding, and lifecycle — in one drawer.', 3000)
    await page.screenshot({ path: join(SHOTS, '05-volume-detail.png') })
    await page.keyboard.press('Escape').catch(() => {})
    await page.waitForTimeout(500)
  }

  // —— Act 4: DataBridge ——
  await gotoPath(page, '/databridge/sources')
  await caption(page, 'DataBridge — cloud-to-edge database migration control plane.', 2800)
  await caption(
    page,
    'Six source engines verified end-to-end: Postgres, MySQL, MariaDB, MongoDB, SQL Server, Oracle.',
    3800,
  )
  await page.screenshot({ path: join(SHOTS, '06-databridge.png') })

  // —— Act 5: Observatory ——
  await gotoPath(page, '/observatory')
  await caption(page, 'Observatory — live client I/O and capacity telemetry.', 3000)
  await page.screenshot({ path: join(SHOTS, '07-observatory.png') })

  // —— Act 6: Ceph ——
  await gotoPath(page, '/ceph')
  await caption(page, 'Ceph, natively — health, OSD tree, and placement groups.', 3200)
  await page.screenshot({ path: join(SHOTS, '08-ceph.png') })
  await page.mouse.wheel(0, 420)
  await page.waitForTimeout(400)
  await caption(page, 'CRUSH map and OSD inventory — no separate Ceph dashboard required.', 3000)
  await page.screenshot({ path: join(SHOTS, '09-ceph-crush.png') })
  await page.mouse.wheel(0, -420)
  await page.waitForTimeout(500)

  // —— Close ——
  await gotoPath(page, '/')
  await caption(page, 'Atlas — survey the cluster before you steer it.', 3000)
  await caption(page, 'zyvor.dev  ·  Atlas Storage Center', 3600)
  await page.screenshot({ path: join(SHOTS, '10-close.png') })

  await page.close()
  await context.close()
  await browser.close()

  const webms = readdirSync(RAW).filter((f) => f.endsWith('.webm'))
  if (!webms.length) throw new Error('no webm recorded')
  const src = join(RAW, webms[0])
  console.log(`[atlas-demo] encoding ${src}`)
  encode(src, FINAL_WEB, 1440, 900)
  encode(src, FINAL_HD, 1920, 1080)

  const elapsed = ((Date.now() - t0) / 1000).toFixed(1)
  console.log(JSON.stringify({ base: BASE, elapsedSec: Number(elapsed), FINAL_WEB, FINAL_HD }, null, 2))
  console.log(`[atlas-demo] DONE in ${elapsed}s`)
}

main().catch((e) => {
  console.error('[atlas-demo] FAIL', e)
  process.exit(1)
})
