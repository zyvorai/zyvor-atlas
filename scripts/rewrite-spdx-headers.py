#!/usr/bin/env python3
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
"""Rewrite Atlas source headers to the LicenseRef-Zyvor-Production-1.0 SPDX form.

Replaces leading Zyvor copyright / "All rights reserved" banners with:
  <comment> Copyright (c) 2026 ZyvorAI Labs Private Limited.
  <comment> SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

Preserves extra banner lines after the copyright (e.g. CSS product blurbs).
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPDX = "LicenseRef-Zyvor-Production-1.0"
COPYRIGHT = "Copyright (c) 2026 ZyvorAI Labs Private Limited."

SKIP_DIRS = {
    ".git",
    "target",
    "node_modules",
    "dist",
    ".docs-tools",
    "LICENSES",  # license texts themselves
}

# Extensions / basenames we rewrite when they already have (or should have) a Zyvor header.
LINE_COMMENT_EXTS = {
    ".rs": "//",
    ".ts": "//",
    ".tsx": "//",
    ".js": "//",
    ".jsx": "//",
    ".mjs": "//",
    ".cjs": "//",
    ".proto": "//",
    ".go": "//",
}
HASH_EXTS = {
    ".toml",
    ".yml",
    ".yaml",
    ".sh",
    ".bash",
    ".py",
    ".env",
    ".ini",
    ".cfg",
    ".conf",
    ".mk",
}
HASH_NAMES = {
    "Makefile",
    "Dockerfile",
    "Dockerfile.ceph",
    "deny.toml",
    ".gitignore",
    ".dockerignore",
    "rustfmt.toml",
}
SQL_EXTS = {".sql"}
MD_EXTS = {".md"}
CSS_EXTS = {".css"}
HTML_EXTS = {".html", ".htm"}

OLD_COPYRIGHT_RE = re.compile(
    r"Copyright \(c\) 2026 ZyvorAI Labs Private Limited\.?(?: All rights reserved\.)?",
    re.IGNORECASE,
)
SPDX_RE = re.compile(r"SPDX-License-Identifier:\s*\S+")


def should_skip(path: Path) -> bool:
    parts = set(path.parts)
    if parts & SKIP_DIRS:
        return True
    if path.name in {"LICENSE", "NOTICE", "package-lock.json", "Cargo.lock"}:
        return True
    if path.suffix.lower() in {".png", ".jpg", ".jpeg", ".gif", ".webp", ".ico", ".pdf", ".bin", ".wasm"}:
        return True
    return False


def style_for(path: Path) -> str | None:
    if path.name in HASH_NAMES or path.name.startswith("Dockerfile"):
        return "hash"
    suf = path.suffix.lower()
    if suf in LINE_COMMENT_EXTS:
        return "line"
    if suf in HASH_EXTS:
        return "hash"
    if suf in SQL_EXTS:
        return "sql"
    if suf in MD_EXTS:
        return "md"
    if suf in CSS_EXTS:
        return "css"
    if suf in HTML_EXTS:
        return "html"
    return None


def rewrite_html(text: str) -> str | None:
    """Rewrite HTML copyright comment near the top (after optional doctype)."""
    lines = text.splitlines(keepends=True)
    if not lines:
        return None
    i = 0
    if lines[0].lstrip().lower().startswith("<!doctype"):
        i = 1
    while i < len(lines) and lines[i].strip() == "":
        i += 1
    if i >= len(lines):
        return None
    m = re.match(r"^(\s*)<!--\s*(.*?)\s*-->\s*$", lines[i].rstrip("\n"))
    if not m:
        return None
    indent = m.group(1)
    content = m.group(2)
    if not OLD_COPYRIGHT_RE.search(content) and "ZyvorAI Labs" not in content:
        if SPDX_RE.search(content):
            return None
        return None
    j = i + 1
    while j < len(lines):
        mm = re.match(r"^\s*<!--\s*(.*?)\s*-->\s*$", lines[j].rstrip("\n"))
        if not mm:
            break
        inner2 = mm.group(1)
        if OLD_COPYRIGHT_RE.search(inner2) or SPDX_RE.search(inner2) or "All rights reserved" in inner2:
            j += 1
            continue
        break
    new_block = [
        f"{indent}<!-- {COPYRIGHT} -->\n",
        f"{indent}<!-- SPDX-License-Identifier: {SPDX} -->\n",
    ]
    return "".join(lines[:i] + new_block + lines[j:])


def rewrite_line_style(text: str, prefix: str) -> str | None:
    """Rewrite // or # or -- style single-line copyright at file start."""
    lines = text.splitlines(keepends=True)
    if not lines:
        return None
    i = 0
    # optional shebang / empty
    if lines[0].startswith("#!"):
        i = 1
    while i < len(lines) and lines[i].strip() == "":
        i += 1
    if i >= len(lines):
        return None

    first = lines[i]
    if not first.lstrip().startswith(prefix.strip() if prefix != "--" else "--"):
        # allow whitespace before comment
        stripped = first.lstrip()
        if not stripped.startswith(prefix):
            return None

    body = first
    # strip comment prefix
    if prefix == "--":
        m = re.match(r"^(\s*--\s?)(.*)$", first)
    else:
        m = re.match(rf"^(\s*{re.escape(prefix)}\s?)(.*)$", first)
    if not m:
        return None
    rest = m.group(2).rstrip("\n")
    if not OLD_COPYRIGHT_RE.search(rest) and "ZyvorAI Labs" not in rest:
        # already SPDX-only? or unrelated first comment
        if SPDX_RE.search(rest):
            return None
        return None

    # Consume contiguous old banner lines that are copyright / all-rights / old spdx
    j = i + 1
    extra: list[str] = []
    while j < len(lines):
        ln = lines[j]
        sm = re.match(rf"^(\s*{re.escape(prefix)}\s?)(.*)$", ln) if prefix != "--" else re.match(
            r"^(\s*--\s?)(.*)$", ln
        )
        if not sm:
            break
        content = sm.group(2).rstrip("\n")
        if OLD_COPYRIGHT_RE.search(content) or content.strip() in {
            "All rights reserved.",
            "Proprietary software — see LICENSE.",
            "https://zyvor.dev",
        }:
            j += 1
            continue
        if SPDX_RE.search(content):
            j += 1
            continue
        # keep other banner comment lines (e.g. module one-liners that were under copyright)
        # but stop if it looks like a normal code doc starting with //! or ///
        if prefix == "//" and content.startswith(("!", "/")):
            break
        # For # style, stop at non-copyright descriptive lines that aren't part of banner —
        # actually keep going only for empty comment lines
        if content.strip() == "":
            j += 1
            continue
        break

    indent = re.match(r"^(\s*)", first).group(1)
    new_block = [
        f"{indent}{prefix} {COPYRIGHT}\n",
        f"{indent}{prefix} SPDX-License-Identifier: {SPDX}\n",
    ]
    new_lines = lines[:i] + new_block + lines[j:]
    return "".join(new_lines)


def rewrite_md(text: str) -> str | None:
    lines = text.splitlines(keepends=True)
    if not lines:
        return None
    first = lines[0]
    m = re.match(r"^<!--\s*(.*?)\s*-->\s*$", first.rstrip("\n"))
    if not m:
        return None
    inner = m.group(1)
    if not OLD_COPYRIGHT_RE.search(inner) and "ZyvorAI Labs" not in inner:
        if SPDX_RE.search(inner):
            return None
        return None
    # Drop following HTML comment lines that only restated proprietary license
    j = 1
    while j < len(lines):
        mm = re.match(r"^<!--\s*(.*?)\s*-->\s*$", lines[j].rstrip("\n"))
        if not mm:
            break
        inner2 = mm.group(1)
        if OLD_COPYRIGHT_RE.search(inner2) or SPDX_RE.search(inner2) or "All rights reserved" in inner2:
            j += 1
            continue
        break
    new_block = [
        f"<!-- {COPYRIGHT} -->\n",
        f"<!-- SPDX-License-Identifier: {SPDX} -->\n",
    ]
    return "".join(new_block + lines[j:])


def rewrite_css(text: str) -> str | None:
    # Multi-line /* ... */ banner at start
    m = re.match(r"^(\s*/\*\s*)(.*?)(\*/\s*)", text, re.DOTALL)
    if not m:
        # try single-line /* Copyright ... */
        m2 = re.match(r"^(\s*/\*.*?\*/\s*\n)", text, re.DOTALL)
        if not m2:
            return None
        block = m2.group(1)
        if "ZyvorAI Labs" not in block and not OLD_COPYRIGHT_RE.search(block):
            return None
        rest = text[m2.end() :]
        # Preserve non-copyright lines from inside the block as a follow-on comment if any
        inner = re.sub(r"^/\*|\*/$", "", block.strip()).strip()
        extras = []
        for line in inner.splitlines():
            s = line.lstrip(" *").strip()
            if not s:
                continue
            if OLD_COPYRIGHT_RE.search(s) or s == "All rights reserved." or SPDX_RE.search(s):
                continue
            extras.append(s)
        header = (
            f"/* {COPYRIGHT}\n"
            f" * SPDX-License-Identifier: {SPDX}\n"
        )
        if extras:
            header += " *\n"
            for e in extras:
                header += f" * {e}\n"
        header += " */\n"
        return header + rest

    block = m.group(0)
    if "ZyvorAI Labs" not in block and not OLD_COPYRIGHT_RE.search(block):
        return None
    rest = text[m.end() :]
    inner = m.group(2)
    extras = []
    for line in inner.splitlines():
        s = line.lstrip(" *").strip()
        if not s:
            continue
        if OLD_COPYRIGHT_RE.search(s) or s == "All rights reserved." or SPDX_RE.search(s):
            continue
        extras.append(s)
    header = f"/* {COPYRIGHT}\n * SPDX-License-Identifier: {SPDX}\n"
    if extras:
        header += " *\n"
        for e in extras:
            header += f" * {e}\n"
    header += " */\n"
    if rest.startswith("\n"):
        return header + rest
    return header + "\n" + rest


def process(path: Path) -> bool:
    style = style_for(path)
    if style is None:
        return False
    try:
        text = path.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        return False

    if style == "line":
        new = rewrite_line_style(text, LINE_COMMENT_EXTS[path.suffix.lower()])
    elif style == "hash":
        new = rewrite_line_style(text, "#")
    elif style == "sql":
        new = rewrite_line_style(text, "--")
    elif style == "md":
        new = rewrite_md(text)
    elif style == "css":
        new = rewrite_css(text)
    elif style == "html":
        new = rewrite_html(text)
    else:
        return False

    if new is None or new == text:
        return False
    path.write_text(new, encoding="utf-8")
    return True


def main() -> int:
    changed = 0
    scanned = 0
    for path in sorted(ROOT.rglob("*")):
        if not path.is_file():
            continue
        if should_skip(path):
            continue
        if style_for(path) is None:
            continue
        scanned += 1
        if process(path):
            changed += 1
            print(f"updated {path.relative_to(ROOT)}")
    print(f"scanned={scanned} updated={changed}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
