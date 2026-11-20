<!-- Copyright (c) 2026 ZyvorAI Labs Private Limited. -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Social assets

The README hero, the GitHub social preview, the LinkedIn/X card and the share cards.

| File | What it is |
|---|---|
| `atlas-hero-dark.html` / `.jpg` | Dark hero, 1200x630 rendered at 2x (2400x1260). The README hero, the docs-site `og:image` (`website/static/img/atlas-social.jpg`) and **the file to upload as the GitHub social preview**. Built by `build-hero-dark.sh` |
| `atlas-social-card.html` / `.jpg` | 1600x900 card for LinkedIn and X: headline, the five-step story, a line from the DR drill. Built by `build-social-card.sh` |
| `atlas-share-card.svg` / `.png` / `@2x.png` | Light 1200x630 card (and 2400x1260), for slides and light backgrounds |
| `atlas-share-card-dark.svg` / `.png` / `@2x.png` | Dark variant of the share card |
| `build-share-cards.py` | Generates both share-card SVGs from one palette |

The README cards (capabilities, native, DR, how it works) live in `docs/ux/readme-*.html` and are
rendered by `docs/ux/build-readme-cards.sh`.

## Palette

| | Light | Dark |
|---|---|---|
| Background | `#ffffff` to `#f5f5f7`, faint blue wash | `#000000` to `#0b0b0f`, blue wash |
| Text | `#1d1d1f`, secondary `#6e6e73` | `#f5f5f7`, secondary `#a1a1a6` |
| Cards | white, hairline `#d2d2d7` | `#1c1c1e`, hairline `#3a3a3c` |
| Accent | blue `#0071e3` to `#2997ff` | blue `#0a84ff` to `#5eb0ff` |

Orange (`#ff6a2a`) appears once, as a small dot on the Ceph card (the primary backend), and nowhere else.
Type is Helvetica Neue and Menlo.

## Rebuild

```bash
./docs/social/build-hero-dark.sh && cp docs/social/atlas-hero-dark.jpg website/static/img/atlas-social.jpg
./docs/social/build-social-card.sh
python3 docs/social/build-share-cards.py docs/social
for v in "" "-dark"; do
  rsvg-convert -w 1200 docs/social/atlas-share-card$v.svg -o docs/social/atlas-share-card$v.png
  rsvg-convert -w 2400 docs/social/atlas-share-card$v.svg -o docs/social/atlas-share-card$v@2x.png
done
```

The HTML cards need Google Chrome and macOS `sips`. The share cards need `rsvg-convert` (librsvg) and Python 3. Edit the stats or the backend list in `build-share-cards.py`;
the numbers must match the code (see the README) and the licence line must match `LICENSE`.

## GitHub social preview

The repository's Social preview image cannot be set through the GitHub API or `gh`. After changing the card,
upload `atlas-hero-dark.jpg` (under GitHub's 1 MB limit) by hand under Settings > General > Social preview.
