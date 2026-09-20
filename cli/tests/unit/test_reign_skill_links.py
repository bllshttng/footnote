"""Every relative markdown link under skills/reign resolves to a file and an anchor.

The one-king-skill fold moved the 549-line pass body one directory deeper,
which is exactly the move that silently breaks relative links. This test walks
every `](target)` in every `*.md` under `skills/reign`, resolves the file part
against the linking file's directory (an anchor-only link targets the file
itself), and slugifies target headings the way GitHub does for `#anchors`.
"""
from __future__ import annotations

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
REIGN = REPO_ROOT / "skills" / "reign"


def _slugify_heading(heading: str) -> str:
    s = re.sub(r"[^a-z0-9 \-]", "", heading.strip().lower())
    return s.replace(" ", "-")


def _link_targets(text: str):
    for match in re.finditer(r"\]\(([^)\s]+)\)", text):
        target = match.group(1)
        if target.startswith(("http", "mailto:")):
            continue
        yield target


def test_every_relative_link_under_reign_resolves():
    checked = 0
    broken = []
    for md in sorted(REIGN.rglob("*.md")):
        for target in _link_targets(md.read_text()):
            checked += 1
            path, _, anchor = target.partition("#")
            dest = (md.parent / path).resolve() if path else md.resolve()
            if not dest.exists():
                broken.append(f"{md}: {target} (missing file)")
                continue
            if anchor and dest.suffix == ".md":
                headings = {
                    _slugify_heading(h)
                    for h in re.findall(r"^#+\s+(.*)$", dest.read_text(), re.M)
                }
                if anchor not in headings:
                    broken.append(f"{md}: {target} (missing anchor)")
    assert not broken, "broken relative links under skills/reign:\n" + "\n".join(broken)
    assert checked > 0, "no links checked; the walk found nothing"
