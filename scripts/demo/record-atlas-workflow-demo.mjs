#!/usr/bin/env node
// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
/**
 * Atlas — "how it works" client demo: actually create, snapshot, and expand a volume
 * (not just a click-through tour). Same recorder/caption pipeline as record-atlas-demo.mjs.
 *
 * The fake driver simulates realistic Ceph resize/snapshot latency (tens of seconds), so this
 * script waits for those jobs for real and cuts to the /jobs page to show live progress instead
 * of faking an instant repaint — expect ~2 minutes end to end.
 *
 * Requires `playwright` (+ Chromium) resolvable from this file's directory — see
 * record-atlas-demo.mjs for setup notes.
 *
 * Usage:
 *   node scripts/demo/record-atlas-workflow-demo.mjs http://<gateway-host>:<port>
 *
 * Env overrides: ATLAS_USER, ATLAS_PASSWORD, ATLAS_DEMO_OUT (default: /tmp/atlas-workflow-demo).
 */
import { mkdirSync, readdirSync, unlinkSync } from 'node:fs'
import { join } from 'node:path'
import { spawnSync } from 'node:child_process'
import { chromium } from 'playwright'

const BASE = (process.argv.find((a) => a.startsWith('http')) || 'http://127.0.0.1:5110').replace(/\/$/, '')
const USER = process.env.ATLAS_USER || 'admin'
const PASS = process.env.ATLAS_PASSWORD || 'Admin@321'
const OUT = process.env.ATLAS_DEMO_OUT || '/tmp/atlas-workflow-demo'
const RAW = join(OUT, 'raw')
const SHOTS = join(OUT, 'shots')
const FINAL_WEB = join(OUT, 'out', 'atlas-create-volume-workflow.mp4')
const FINAL_HD = join(OUT, 'out', 'atlas-create-volume-workflow-1080p.mp4')
const VOL_NAME = `wow-demo-${Date.now().toString(36).slice(-6)}`

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
        '<div id="atlas-demo-caption-brand">Atlas &middot; How It Works</div><div id="atlas-demo-caption-text"></div>'
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

/** Type into a field after clearing it (Field has no id/for, so we locate by handle). */
async function fillField(locator, value) {
  await locator.click()
  await locator.fill('')
  await locator.type(value, { delay: 35 })
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
  console.log(`[atlas-workflow] base=${BASE} volume=${VOL_NAME}`)

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
  await caption(page, 'Atlas — how it works. Let’s provision a volume end to end.', 3000)
  await page.fill('#atlas-username', USER)
  await page.fill('#atlas-password', PASS)
  await page.click('button:has-text("Sign in to Atlas")')
  await page.waitForTimeout(2000)
  await dismissNoise(page)
  await ensureCaptionCss(page)

  // —— Act 2: Volumes list (before) ——
  await gotoPath(page, '/volumes')
  await caption(page, 'Volumes — here’s the fleet before we start.', 2600)
  await page.screenshot({ path: join(SHOTS, '01-before.png') })

  // —— Act 3: Create volume ——
  const newVolumeBtn = page.getByRole('button', { name: /^\+?\s*Volume$/ }).first()
  await newVolumeBtn.click()
  await page.waitForTimeout(700)
  await caption(page, 'New Volume — request intent, not a pool name.', 2600)

  const nameField = page.getByPlaceholder('my-volume')
  await fillField(nameField, VOL_NAME)
  await caption(page, `Name it "${VOL_NAME}"…`, 1800)

  const sizeField = page.locator('.at-glass input[type="number"]').first()
  await fillField(sizeField, '25')
  await caption(page, '…give it 25 GiB…', 1800)

  const policySelect = page.locator('.at-glass select').first()
  await policySelect.selectOption('production')
  await caption(page, '…and tag the policy "production" — Atlas maps that to a backend and pool.', 3200)
  await page.screenshot({ path: join(SHOTS, '02-create-form.png') })

  await page.locator('.at-glass').getByRole('button', { name: 'Create', exact: true }).click()
  await caption(page, 'Every write returns 202 + a tracked job — nothing blocks, nothing is lost on restart.', 3600)
  await page.waitForTimeout(1500)
  await page.screenshot({ path: join(SHOTS, '03-created-toast.png') })

  // —— Act 4: New volume appears ——
  const row = page.locator('tr', { hasText: VOL_NAME }).first()
  await row.waitFor({ state: 'visible', timeout: 15000 }).catch(() => {})
  await caption(page, 'There it is — provisioned and online.', 2600)
  await page.screenshot({ path: join(SHOTS, '04-after-create.png') })

  // —— Act 5: Open the drawer ——
  await row.click()
  await page.waitForTimeout(900)
  await caption(page, 'Full detail: ID, class, PVC binding, and lifecycle — one drawer, no kubectl.', 3200)
  await page.screenshot({ path: join(SHOTS, '05-drawer.png') })
  await page.keyboard.press('Escape').catch(() => {})
  await page.waitForTimeout(500)

  // —— Act 6: Snapshot it ——
  await row.getByRole('button', { name: 'Snap' }).click()
  await page.waitForTimeout(700)
  await caption(page, 'Snapshot it — one click, no rbd CLI.', 2600)
  await page.screenshot({ path: join(SHOTS, '06-snapshot-form.png') })
  await page.locator('.at-glass').getByRole('button', { name: 'Snapshot', exact: true }).click()
  await caption(page, 'Point-in-time copy, ready to clone or restore later.', 2600)
  await page.waitForTimeout(600)
  await page.screenshot({ path: join(SHOTS, '07-snapshot-done.png') })

  // —— Act 7: Expand it live ——
  await row.getByRole('button', { name: 'Expand' }).click()
  await page.waitForTimeout(700)
  const expandField = page.locator('.at-glass input[type="number"]').first()
  await fillField(expandField, '40')
  await caption(page, 'Grow it live to 40 GiB — expand-only guard built in, no downtime.', 3400)
  await page.screenshot({ path: join(SHOTS, '08-expand-form.png') })
  await page.locator('.at-glass').getByRole('button', { name: 'Expand', exact: true }).click()
  await page.waitForTimeout(1200)

  // —— Act 7b: Watch both jobs reconcile live (Ceph resize genuinely takes tens of seconds —
  // show that honestly instead of faking an instant repaint) ——
  await caption(page, 'Every write returns a job — let’s watch it reconcile live.', 2800)
  await gotoPath(page, '/jobs')
  await caption(page, 'Snapshot and expand, both tracked — polled every few seconds, nothing lost on restart.', 3600)
  await page.screenshot({ path: join(SHOTS, '09-jobs-live.png') })
  await page.waitForTimeout(18000)
  await page.screenshot({ path: join(SHOTS, '10-jobs-progress.png') })
  const bothDone = await page
    .locator('tr', { hasText: /succeeded/i })
    .filter({ hasText: /expand|snapshot/i })
    .count()
    .then((n) => n >= 2)
    .catch(() => false)
  if (!bothDone) await page.waitForTimeout(22000)
  await caption(page, 'Done — bigger, without touching a single YAML file.', 3000)
  await page.screenshot({ path: join(SHOTS, '11-jobs-done.png') })

  // —— Act 8: Confirm the resize back on Volumes ——
  let resized = false
  for (let i = 0; i < 4 && !resized; i++) {
    await gotoPath(page, '/volumes')
    resized = await row.locator('td', { hasText: '40.0' }).isVisible().catch(() => false)
    if (!resized) await page.waitForTimeout(3000)
  }
  await caption(page, `${VOL_NAME} — 25 GiB to 40 GiB, live.`, 2800)
  await page.screenshot({ path: join(SHOTS, '12-expand-confirmed.png') })

  // —— Close: back to Command Deck ——
  await gotoPath(page, '/')
  await caption(page, 'Create, protect, and grow storage — without leaving the browser.', 3200)
  await caption(page, 'Atlas — survey the cluster before you steer it. zyvor.dev', 3600)
  await page.screenshot({ path: join(SHOTS, '13-close.png') })

  await page.close()
  await context.close()
  await browser.close()

  const webms = readdirSync(RAW).filter((f) => f.endsWith('.webm'))
  if (!webms.length) throw new Error('no webm recorded')
  const src = join(RAW, webms[0])
  console.log(`[atlas-workflow] encoding ${src}`)
  encode(src, FINAL_WEB, 1440, 900)
  encode(src, FINAL_HD, 1920, 1080)

  const elapsed = ((Date.now() - t0) / 1000).toFixed(1)
  console.log(JSON.stringify({ base: BASE, volume: VOL_NAME, elapsedSec: Number(elapsed), FINAL_WEB, FINAL_HD }, null, 2))
  console.log(`[atlas-workflow] DONE in ${elapsed}s`)
}

main().catch((e) => {
  console.error('[atlas-workflow] FAIL', e)
  process.exit(1)
})
