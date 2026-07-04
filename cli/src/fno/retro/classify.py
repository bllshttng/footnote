"""Classify + expand harvested items into node/inbox candidates.

Deterministic and verbatim-preserving by design (Discretion #4/#7 resolution):
the node body carries the originating reasoning UNCHANGED plus a source cite, and
the title is derived from the finding's own first line - never a generic stub and
never an LLM paraphrase that could drift from the source. A candidate that cannot
be tied to a source_pr + source id is rejected (anti-hallucination: no cite, no
node), surfaced as ``uncited`` for the caller to log as ``triage_uncited_candidate``.
"""
from __future__ import annotations

import re

from fno.retro.types import (
    KIND_CARVEOUT,
    TIER_INBOX,
    TIER_NODE,
    Candidate,
    RawItem,
)

# Severity -> tier (Locked Decision #5): critical/high/medium -> node; low -> inbox.
_NODE_SEVERITIES = {"critical", "high", "medium"}
# Severity -> priority pN.
_SEVERITY_PRIORITY = {"critical": "p0", "high": "p1", "medium": "p2", "low": "p3"}

DEFAULT_PRIORITY = "p3"
TITLE_CAP = 100
BODY_CAP = 6000

_BADGE_RE = re.compile(r"!\[[^\]]*\]\([^)]*\)|!\[[^\]]*\]")  # ![alt](url) or ![alt]
_MD_LEAD = re.compile(r"^[#>\-\*\s`]+")


def severity_to_priority(severity: str | None) -> str:
    return _SEVERITY_PRIORITY.get((severity or "").lower(), DEFAULT_PRIORITY)


def severity_to_tier(severity: str | None) -> str:
    """node for critical/high/medium (and severity-less deliberate work); inbox for low."""
    if severity is None:
        return TIER_NODE  # carve-outs / deferred findings are deliberate work
    return TIER_NODE if severity.lower() in _NODE_SEVERITIES else TIER_INBOX


def _clean_line(text: str) -> str:
    for raw in text.splitlines():
        stripped = _BADGE_RE.sub("", raw).strip()
        stripped = _MD_LEAD.sub("", stripped).strip()
        if stripped:
            return stripped
    return ""


def derive_title(item: RawItem) -> str:
    """One-line summary derived from the finding's own text (never a generic stub)."""
    if item.kind == KIND_CARVEOUT:
        summary = (item.title_hint or "").strip() or _clean_line(item.text)
        if item.subkind == "oos-bug":
            summary = f"bug: {summary}" if not summary.lower().startswith("bug") else summary
    else:
        summary = _clean_line(item.text)
    summary = summary or "(untitled left-out item)"
    if len(summary) > TITLE_CAP:
        summary = summary[: TITLE_CAP - 1].rstrip() + "…"
    return summary


def _resolve_priority(item: RawItem) -> str:
    if item.priority:
        return item.priority
    if item.severity:
        return severity_to_priority(item.severity)
    return DEFAULT_PRIORITY


def build_body(item: RawItem, *, cap: int = BODY_CAP) -> str:
    """Verbatim reasoning + a source cite. Truncates the reasoning past the cap."""
    reasoning = item.text or ""
    cite_bits = []
    if item.source_pr is not None:
        cite_bits.append(f"PR #{item.source_pr}")
    if item.reviewer:
        cite_bits.append(f"reviewer `{item.reviewer}`")
    if item.url:
        cite_bits.append(item.url)
    elif item.source_id:
        cite_bits.append(f"source `{item.source_id}`")
    cite = "Source: " + ", ".join(cite_bits) if cite_bits else ""

    # Reserve exact room for the cite + truncation marker so the whole body fits
    # the cap (the marker length is known, so the final body cannot overflow).
    link = item.url or (f"PR #{item.source_pr}" if item.source_pr else "source")
    marker = f"\n\n[... truncated; full text at {link} ...]"
    overhead = len(marker) + (len(cite) + 2 if cite else 0)
    budget = max(cap - overhead, 200)
    if len(reasoning) > budget:
        reasoning = reasoning[:budget].rstrip() + marker

    parts = [reasoning.strip()]
    if cite:
        parts.append(cite)
    return "\n\n".join(p for p in parts if p)


def classify_item(item: RawItem, *, body_cap: int = BODY_CAP) -> Candidate:
    """Turn one RawItem into a Candidate. Marks ``uncited`` when it has no cite."""
    cited = item.source_pr is not None and bool(item.source_id)
    if not cited:
        return Candidate(
            title=derive_title(item),
            body=item.text or "",
            tier=TIER_NODE,
            priority=DEFAULT_PRIORITY,
            source_pr=item.source_pr,
            source_id=item.source_id,
            uncited=True,
        )

    tier = severity_to_tier(item.severity)
    return Candidate(
        title=derive_title(item),
        body=build_body(item, cap=body_cap),
        tier=tier,
        priority=_resolve_priority(item),
        source_pr=item.source_pr,
        source_id=item.source_id,
        finding_text=item.text or "",
        extra={
            "kind": item.kind,
            "subkind": item.subkind,
            "severity": item.severity,
            "reviewer": item.reviewer,
        },
    )


# ── postmortem disposition (W6 6.2, rule-first per Discretion #7) ────────────

DISPOSITION_NODE = "node"
DISPOSITION_INBOX = "inbox"
DISPOSITION_ARCHIVE = "archive"

# One-off: a specific cancel / interrupt / transient - archived, no work filed.
_PM_ONEOFF_RE = re.compile(r"cancel|interrupt|abort", re.IGNORECASE)
# Wedge-class: a named, repeatable failure mode worth a backlog node.
_PM_WEDGE_RE = re.compile(
    r"split.?brain|wedge|respawn\s+loop|orphan|deadlock|"
    r"stale\W+(\w+\W+){0,3}claim|claim\W+(\w+\W+){0,3}stale|budget\s+blowout",
    re.IGNORECASE,
)


def classify_postmortem(item: RawItem, *, body_cap: int = BODY_CAP) -> "tuple[str, Candidate | None]":
    """Rule-first disposition for one postmortem: exactly one of
    (node, inbox, archive). Ambiguous -> inbox: surface, don't guess.

    Returns ``(disposition, candidate)``; candidate is None for archive.
    Postmortems have no PR, so candidates are built directly (the cite is the
    postmortem file itself, carried in source_id) rather than through the
    PR-cited classify_item path.
    """
    reason = item.subkind or ""
    if _PM_ONEOFF_RE.search(reason) or (not reason and _PM_ONEOFF_RE.search(item.text or "")):
        return DISPOSITION_ARCHIVE, None

    wedge = bool(_PM_WEDGE_RE.search(item.text or ""))
    tier = TIER_NODE if wedge else TIER_INBOX
    title = (item.title_hint or "").strip() or _clean_line(item.text)
    title = title or "(untitled postmortem)"
    if len(title) > TITLE_CAP:
        title = title[: TITLE_CAP - 1].rstrip() + "…"

    body = (item.text or "").strip()
    marker = "\n\n[... truncated; full text in the postmortem file ...]"
    cite = f"Source: {item.source_id}"
    budget = max(body_cap - len(marker) - len(cite) - 2, 200)
    if len(body) > budget:
        body = body[:budget].rstrip() + marker
    return (
        DISPOSITION_NODE if wedge else DISPOSITION_INBOX,
        Candidate(
            title=title,
            body=f"{body}\n\n{cite}" if body else cite,
            tier=tier,
            priority="p2" if wedge else DEFAULT_PRIORITY,
            source_pr=None,
            source_id=item.source_id,
            finding_text=item.text or "",
            extra={"kind": item.kind, "subkind": item.subkind},
        ),
    )


def classify(items: list[RawItem], *, body_cap: int = BODY_CAP) -> "tuple[list[Candidate], list[Candidate]]":
    """Classify all items. Returns (cited_candidates, uncited_rejects)."""
    cited: list[Candidate] = []
    uncited: list[Candidate] = []
    for it in items:
        c = classify_item(it, body_cap=body_cap)
        (uncited if c.uncited else cited).append(c)
    return cited, uncited
