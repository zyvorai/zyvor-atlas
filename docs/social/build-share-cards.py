# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
"""Generate the Atlas share cards (light + dark) as SVG from one palette.

    python3 docs/social/build-share-cards.py docs/social
    rsvg-convert -w 1200 docs/social/atlas-share-card.svg      -o docs/social/atlas-share-card.png
    rsvg-convert -w 1200 docs/social/atlas-share-card-dark.svg -o docs/social/atlas-share-card-dark.png
"""
import sys

LIGHT = dict(
    bg0="#ffffff", bg1="#f5f5f7", wash="#0071e3", wash_op="0.10", wash2_op="0.05",
    ink="#1d1d1f", sec="#6e6e73", card="#ffffff", card_stroke="#d2d2d7", shadow_op="0.10",
    blue0="#0071e3", blue1="#2997ff", link="#0071e3", wire="#0071e3", api_sub="#dcecff",
    card_hi="#0071e3", hair="#e5e5ea")
DARK = dict(
    bg0="#000000", bg1="#0b0b0f", wash="#2997ff", wash_op="0.20", wash2_op="0.08",
    ink="#f5f5f7", sec="#a1a1a6", card="#1c1c1e", card_stroke="#3a3a3c", shadow_op="0.55",
    blue0="#0a84ff", blue1="#5eb0ff", link="#2997ff", wire="#2997ff", api_sub="#d6e9ff",
    card_hi="#2997ff", hair="#2c2c2e")

SANS = "'Helvetica Neue',Helvetica,Arial,sans-serif"
MONO = "'Menlo','JetBrains Mono',monospace"

TAGLINE = ("One API for your storage.", "A filesystem of its own.")

# (name, detail); Ceph is the primary backend and carries the single orange dot. Object storage
# defaults to Ceph RGW (no separate first-party object backend since RustFS was removed).
BACKENDS = [
    ("Ceph", "RBD · CephFS · RGW"),
    ("NFS", "file shares"),
    ("ZFS", "pools"),
    ("Longhorn", "Kubernetes volumes"),
    ("Native", "EC · NFS · SMB · S3"),
]
STATS = [("4", "storage backends"), ("5", "native access paths"),
         ("6", "database engines"), ("150+", "REST endpoints")]
NEW_BADGE = "NEW · two-way DR, clean failback"


def svg(p, label):
    card_h, gap, x0, w = 56, 14, 936, 192
    n = len(BACKENDS)
    top = 315 - (n * card_h + (n - 1) * gap) // 2
    cards, wires, dots = [], [], []
    for i, (name, detail) in enumerate(BACKENDS):
        y = top + i * (card_h + gap)
        cy = y + card_h // 2
        hi = i == 0
        stroke = (f'stroke="{p["card_hi"]}" stroke-opacity="0.75" stroke-width="1.5"' if hi
                  else f'stroke="{p["card_stroke"]}"')
        cards.append(
            f'<rect x="{x0}" y="{y}" width="{w}" height="{card_h}" rx="15" fill="{p["card"]}" {stroke} filter="url(#shadow)"/>\n'
            f'      <text x="{x0 + 20}" y="{y + 25}" font-size="19" font-weight="700" fill="{p["ink"]}">{name}</text>\n'
            f'      <text x="{x0 + 20}" y="{y + 44}" font-size="12.5" font-family="{MONO}" fill="{p["sec"]}">{detail}</text>')
        wires.append(f'<path d="M872 315 C 900 315, 900 {cy}, {x0 - 12} {cy}"/>')
        dots.append(f'<circle cx="{x0 - 12}" cy="{cy}" r="4.5"/>')
        if hi:
            cards.append(f'<circle cx="{x0 + w - 20}" cy="{y + 20}" r="5" fill="#ff6a2a"/>')
    stats = []
    for i, (num, lab) in enumerate(STATS):
        x = 72 + i * 162
        stats.append(
            f'<text x="{x}" y="498" font-size="38" font-weight="700" letter-spacing="-1" fill="{p["ink"]}">{num}</text>\n'
            f'    <text x="{x}" y="524" font-size="15" fill="{p["sec"]}">{lab}</text>')
    return f'''<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="630" viewBox="0 0 1200 630" role="img" aria-label="{label}">
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0" y2="630" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="{p['bg0']}"/>
      <stop offset="1" stop-color="{p['bg1']}"/>
    </linearGradient>
    <radialGradient id="wash" cx="930" cy="300" r="520" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="{p['wash']}" stop-opacity="{p['wash_op']}"/>
      <stop offset="0.6" stop-color="{p['wash']}" stop-opacity="{p['wash2_op']}"/>
      <stop offset="1" stop-color="{p['wash']}" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="blue" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="{p['blue0']}"/>
      <stop offset="1" stop-color="{p['blue1']}"/>
    </linearGradient>
    <linearGradient id="blueText" x1="72" y1="0" x2="420" y2="0" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="{p['blue0']}"/>
      <stop offset="1" stop-color="{p['blue1']}"/>
    </linearGradient>
    <linearGradient id="wire" x1="0" y1="0" x2="1" y2="0">
      <stop offset="0" stop-color="{p['wire']}" stop-opacity="0.9"/>
      <stop offset="1" stop-color="{p['wire']}" stop-opacity="0.35"/>
    </linearGradient>
    <filter id="shadow" x="-20%" y="-30%" width="140%" height="190%" color-interpolation-filters="sRGB">
      <feGaussianBlur in="SourceAlpha" stdDeviation="8"/>
      <feOffset dy="7" result="b"/>
      <feColorMatrix in="b" type="matrix" values="0 0 0 0 0  0 0 0 0 0  0 0 0 0 0  0 0 0 {p['shadow_op']} 0" result="s"/>
      <feMerge><feMergeNode in="s"/><feMergeNode in="SourceGraphic"/></feMerge>
    </filter>
    <filter id="glow" x="-30%" y="-40%" width="160%" height="200%" color-interpolation-filters="sRGB">
      <feGaussianBlur in="SourceAlpha" stdDeviation="13"/>
      <feOffset dy="11" result="b"/>
      <feColorMatrix in="b" type="matrix" values="0 0 0 0 0.0  0 0 0 0 0.35  0 0 0 0 0.9  0 0 0 0.30 0" result="s"/>
      <feMerge><feMergeNode in="s"/><feMergeNode in="SourceGraphic"/></feMerge>
    </filter>
  </defs>

  <rect width="1200" height="630" fill="url(#bg)"/>
  <rect width="1200" height="630" fill="url(#wash)"/>

  <!-- Zyvor mark (blue) -->
  <rect x="72" y="56" width="56" height="56" rx="13" fill="url(#blue)"/>
  <path d="M86.5 70 113.5 70 86.5 98 113.5 98" fill="none" stroke="#ffffff" stroke-width="7"
        stroke-linecap="round" stroke-linejoin="round"/>

  <!-- wordmark + tagline -->
  <text x="68" y="262" font-family="{SANS}" font-size="132" font-weight="700" letter-spacing="-4" fill="{p['ink']}">Atlas</text>
  <text x="72" y="316" font-family="{SANS}" font-size="42" font-weight="600" letter-spacing="-0.8" fill="url(#blueText)">{TAGLINE[0]}</text>
  <text x="72" y="362" font-family="{SANS}" font-size="42" font-weight="600" letter-spacing="-0.8" fill="url(#blueText)">{TAGLINE[1]}</text>

  <!-- new: badge -->
  <rect x="72" y="398" width="330" height="34" rx="17" fill="{p['wash']}" fill-opacity="{p['wash_op']}" stroke="{p['card_hi']}" stroke-opacity="0.4"/>
  <text x="87" y="420" font-family="{MONO}" font-size="15" font-weight="700" fill="{p['card_hi']}">{NEW_BADGE}</text>

  <!-- verified stats -->
  <g font-family="{SANS}">
    {chr(10).join('    ' + s if i else s for i, s in enumerate(stats))}
  </g>

  <!-- one API fans out to five backends -->
  <g fill="none" stroke="url(#wire)" stroke-width="2.5" stroke-linecap="round">
    {chr(10).join('    ' + s if i else s for i, s in enumerate(wires))}
  </g>
  <g fill="{p['wire']}">
    <circle cx="872" cy="315" r="5.5"/>
    {chr(10).join('    ' + s for s in dots)}
  </g>
  <rect x="706" y="258" width="166" height="114" rx="26" fill="url(#blue)" filter="url(#glow)"/>
  <text x="789" y="308" text-anchor="middle" font-family="{SANS}" font-size="27" font-weight="700" fill="#ffffff">Atlas API</text>
  <text x="789" y="340" text-anchor="middle" font-family="{MONO}" font-size="14" fill="{p['api_sub']}">REST · gRPC · SSE</text>
  <g font-family="{SANS}">
    {chr(10).join('    ' + s if i else s for i, s in enumerate(cards))}
  </g>

  <!-- footer -->
  <line x1="72" y1="560" x2="1128" y2="560" stroke="{p['hair']}" stroke-width="1"/>
  <text x="72" y="596" font-family="{MONO}" font-size="16" fill="{p['sec']}">github.com/zyvorai/zyvor-atlas</text>
  <text x="1128" y="596" text-anchor="end" font-family="{SANS}" font-size="16" fill="{p['sec']}">v0.5.0 · Apache License 2.0</text>
</svg>
'''


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else "docs/social"
    label = (f"Atlas — {TAGLINE[0]} {TAGLINE[1]} One API in front of Ceph, NFS, ZFS and Longhorn, "
              "plus Atlas Native, a distributed filesystem with NFS, SMB and S3.")
    open(f"{out}/atlas-share-card.svg", "w").write(svg(LIGHT, label))
    open(f"{out}/atlas-share-card-dark.svg", "w").write(svg(DARK, label))
