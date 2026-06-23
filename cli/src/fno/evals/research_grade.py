"""Research eval: the three mechanical assertions that make a brief "green" (US5).

Code-ship has external truth (PR + CI + bot). Research never did, so "done" was
a vibe. This is that truth, and it is purely mechanical - no model in the gate:

  (a) zero uncited claims  - every claim cites a [Sn] marker that resolves, via
      the brief's Sources section, to a URL that is a row in the sidecar
      sources.jsonl (AC1, AC2a).
  (b) zero dead URLs       - every sidecar row is verified after a self-fetch,
      or is a Wayback (web.archive.org) URL (AC2b).
  (c) >=1 golden checklist item per section - the golden discovery-*.md doc's
      headings are the checklist; each brief content section must cover at least
      one (AC2c).

The brief is green only if all three pass. The research-verify panel (US4) is
advisory and never consulted here - it annotates, it never changes this verdict.

A "claim" is a markdown list item under a content section (any `## ` section
other than Sources). "Covers" is a normalized-substring match of a golden
heading against the section text - a deterministic floor, not a judge.
"""
from __future__ import annotations

import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from fno.research.core import read_sources

_CLAIM_RE = re.compile(r"^\s*[-*]\s+(.+)$")
_CITE_RE = re.compile(r"\[S(\d+)\]")
_REF_RE = re.compile(r"^\s*\[S(\d+)\]:\s*(\S+)", re.MULTILINE)
_H2_RE = re.compile(r"^##\s+(.+?)\s*#*$")
_HEADING_RE = re.compile(r"^#{2,6}\s+(.+?)\s*#*$")
_NORM_RE = re.compile(r"[^a-z0-9]+")


class GradeError(RuntimeError):
    """Unrecoverable scorer setup error (missing brief / golden / sidecar)."""


@dataclass
class GradeResult:
    brief: str
    golden: str
    sidecar: str
    uncited_claims: int = 0
    dead_urls: int = 0
    sections_uncovered: list[str] = field(default_factory=list)
    detail: list[str] = field(default_factory=list)

    @property
    def green(self) -> bool:
        return (
            self.uncited_claims == 0
            and self.dead_urls == 0
            and not self.sections_uncovered
        )

    def summary(self) -> str:
        verdict = "GREEN" if self.green else "RED"
        lines = [
            f"research grade: {verdict}",
            f"  (a) uncited claims:     {self.uncited_claims}",
            f"  (b) dead source URLs:   {self.dead_urls}",
            f"  (c) uncovered sections: {len(self.sections_uncovered)}"
            + (f" ({', '.join(self.sections_uncovered)})" if self.sections_uncovered else ""),
        ]
        lines += [f"    - {d}" for d in self.detail]
        return "\n".join(lines)


def _normalize(s: str) -> str:
    return _NORM_RE.sub(" ", s.lower()).strip()


def _strip_frontmatter(text: str) -> tuple[dict[str, str], str]:
    """Split a leading `---` frontmatter block. Returns (fields, body).

    Only the flat scalar keys we need (`sources:`) are parsed; the rest is
    ignored - this is not a full YAML parser.
    """
    fields: dict[str, str] = {}
    if text.startswith("---"):
        end = text.find("\n---", 3)
        if end != -1:
            fm = text[3:end]
            for line in fm.splitlines():
                m = re.match(r"^\s*([A-Za-z0-9_]+):\s*(.+?)\s*$", line)
                if m:
                    fields[m.group(1)] = m.group(2).strip().strip('"').strip("'")
            body = text[end + 4 :]
            return fields, body
    return fields, text


def _content_sections(body: str) -> list[tuple[str, str]]:
    """Return (name, section_text) for each `## ` section except Sources."""
    sections: list[tuple[str, list[str]]] = []
    current: Optional[str] = None
    for line in body.splitlines():
        m = _H2_RE.match(line)
        if m:
            current = m.group(1).strip()
            if current.lower() == "sources":
                current = None  # skip the references block
                continue
            sections.append((current, []))
        elif current is not None and sections:
            sections[-1][1].append(line)
    return [(name, "\n".join(lines)) for name, lines in sections]


def _golden_checklist(golden_text: str) -> list[str]:
    _, body = _strip_frontmatter(golden_text)
    items: list[str] = []
    for line in body.splitlines():
        m = _HEADING_RE.match(line)
        if m:
            items.append(m.group(1).strip())
    return items


def _resolve_sidecar(brief_path: Path, fields: dict[str, str], sidecar_path: Optional[Path]) -> Path:
    if sidecar_path is not None:
        return sidecar_path
    named = fields.get("sources")
    if named:
        return brief_path.parent / named
    return brief_path.parent / f"{brief_path.stem}.sources.jsonl"


def grade(
    brief_path: Path | str,
    golden_path: Path | str,
    *,
    sidecar_path: Optional[Path | str] = None,
) -> GradeResult:
    """Score a brief against a golden doc via the three mechanical assertions."""
    brief_path = Path(brief_path)
    golden_path = Path(golden_path)
    if not brief_path.is_file():
        raise GradeError(f"brief not found: {brief_path}")
    if not golden_path.is_file():
        raise GradeError(f"golden doc not found: {golden_path}")

    brief_text = brief_path.read_text(encoding="utf-8")
    golden_text = golden_path.read_text(encoding="utf-8")
    fields, body = _strip_frontmatter(brief_text)

    sidecar = _resolve_sidecar(brief_path, fields, Path(sidecar_path) if sidecar_path else None)
    if not sidecar.exists():
        raise GradeError(f"sources sidecar not found: {sidecar}")

    rows = read_sources(sidecar)
    sidecar_urls = {r.url for r in rows}
    # ref map [Sn] -> url (parsed across the whole brief, incl. the Sources block)
    refs = {int(n): url for n, url in _REF_RE.findall(brief_text)}

    res = GradeResult(brief=str(brief_path), golden=str(golden_path), sidecar=str(sidecar))

    # --- (a) zero uncited claims ------------------------------------------- #
    sections = _content_sections(body)
    for name, sect in sections:
        for line in sect.splitlines():
            m = _CLAIM_RE.match(line)
            if not m:
                continue
            cites = [int(n) for n in _CITE_RE.findall(line)]
            linked = [n for n in cites if n in refs and refs[n] in sidecar_urls]
            if not linked:
                res.uncited_claims += 1
                res.detail.append(f"uncited claim in '{name}': {m.group(1)[:60]}")

    # --- (b) zero dead source URLs ----------------------------------------- #
    for r in rows:
        resolvable = r.verified or ("web.archive.org" in r.url)
        if not resolvable:
            res.dead_urls += 1
            res.detail.append(f"dead source: {r.url} ({r.reason or 'unverified'})")

    # --- (c) >=1 golden checklist item per section ------------------------- #
    # A brief with no content sections covers nothing - red (AC3: a no-sources
    # brief grades red on (c), it is not vacuously green).
    checklist = [_normalize(item) for item in _golden_checklist(golden_text)]
    checklist = [c for c in checklist if c]
    if not sections:
        res.sections_uncovered.append("(no content sections)")
    for name, sect in sections:
        hay = _normalize(name + " " + sect)
        if not any(item in hay for item in checklist):
            res.sections_uncovered.append(name)

    return res
