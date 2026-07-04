"""Deterministic mechanical guards.

Pure functions, no I/O - they run before and after the synthesis agent, never
instead of it. Every guard is line-count math or a substring scan, not an LLM
judgment call: the human merge gate is the single point of control, so guards
flag (bloat) or drop (uncited hunks) or refuse (redaction) but never "decide"
quality.

A ``hunk`` is ``{file, old_text, new_text, cited_finding_ids, rationale}``:
old_text="" is a pure addition; a non-empty old_text is a modification.
"""
from __future__ import annotations

from typing import Iterable, Optional

# Tunable defaults (Claude's Discretion #2). Constants, not config: changing one
# is a one-line edit, not a design change. Raise them here if a skill's edits
# legitimately run large.
ADDITIVE_LINE_THRESHOLD = 15  # additive-only diffs adding more than this need justification
BLOAT_WINDOW = 5  # trailing skill_diff_proposed events per skill
BLOAT_NET_GROWTH_THRESHOLD = 120  # net (added-removed) lines over the window that flags


def _lines(text: str) -> int:
    """Count non-empty content lines. A blank string is zero lines, not one."""
    if not text:
        return 0
    return len([ln for ln in text.splitlines() if ln.strip()])


def count_lines(hunks: Iterable[dict]) -> tuple[int, int]:
    """(added, removed) across all hunks. removed==0 means additive-only."""
    added = removed = 0
    for h in hunks:
        added += _lines(h.get("new_text", ""))
        removed += _lines(h.get("old_text", ""))
    return added, removed


def filter_cited_hunks(hunks: Iterable[dict]) -> tuple[list[dict], list[dict]]:
    """Drop hunks that cite zero findings (AC2-ERR).

    Returns (kept, dropped). Earned-specificity applied to skill edits: every
    surviving hunk points at a specific observed failure. A caller that gets an
    empty ``kept`` takes the no-diff-helps path rather than opening an empty PR.
    """
    kept, dropped = [], []
    for h in hunks:
        cites = [c for c in (h.get("cited_finding_ids") or []) if c]
        (kept if cites else dropped).append(h)
    return kept, dropped


def additive_only_needs_justification(
    hunks: Iterable[dict], *, threshold: int = ADDITIVE_LINE_THRESHOLD
) -> bool:
    """True when the diff only adds lines and adds more than *threshold* (AC4-UI).

    Such a diff is suspect (a proposer that only ever appends bloats a skill),
    so it may only open a PR if the synthesis output carried a justification.
    """
    added, removed = count_lines(hunks)
    return removed == 0 and added > threshold


def bloat_flag(
    prior_proposed: list[dict],
    *,
    window: int = BLOAT_WINDOW,
    threshold: int = BLOAT_NET_GROWTH_THRESHOLD,
) -> Optional[dict]:
    """Cumulative bloat over a trailing window of this skill's prior proposals (AC5-UI).

    *prior_proposed* is this skill's ``skill_diff_proposed`` event ``data`` dicts
    in append order (oldest first). Derived, not stored: sum
    net (added-removed) growth over the last *window* proposals. Flags, never
    blocks - returns a dict rendered into the PR body, or None.
    """
    recent = prior_proposed[-window:]
    net = sum(int(d.get("added_lines", 0)) - int(d.get("removed_lines", 0)) for d in recent)
    if net > threshold:
        return {"net_growth": net, "window": len(recent), "threshold": threshold}
    return None


def redaction_violations(text: str, project_names: Iterable[str]) -> list[str]:
    """Scan PR-body / commit-message text for surfaces that must never go public (A3).

    Proposer PRs land on the PUBLIC footnote repo but cite findings distilled
    from a corpus spanning ALL fno-enabled projects, including private client
    work. ``check-no-internal-refs`` CI scans committed files, but PR bodies and
    commit messages are GitHub metadata it never sees - this is that gap's guard.
    Refuse the open on any hit. Opaque finding IDs remain the citation mechanism.
    """
    hits: list[str] = []
    for needle in ("internal/", "~/.fno", "/.fno/"):
        if needle in text:
            hits.append(needle)
    lowered = text.lower()
    for name in project_names:
        name = (name or "").strip()
        if not name or name == "fno":
            continue
        # Word-ish boundary check keeps a short project name from matching inside
        # an unrelated word; a bare substring scan would be too trigger-happy.
        if _contains_token(lowered, name.lower()):
            hits.append(f"project:{name}")
    return hits


def _contains_token(haystack: str, token: str) -> bool:
    import re

    return re.search(rf"(?<![a-z0-9]){re.escape(token)}(?![a-z0-9])", haystack) is not None


if __name__ == "__main__":  # pragma: no cover - smoke self-check
    kept, dropped = filter_cited_hunks(
        [{"cited_finding_ids": ["f1"], "new_text": "a\nb"}, {"cited_finding_ids": [], "new_text": "c"}]
    )
    assert len(kept) == 1 and len(dropped) == 1, (kept, dropped)
    assert additive_only_needs_justification([{"old_text": "", "new_text": "x\n" * 20}])
    assert not additive_only_needs_justification([{"old_text": "y", "new_text": "x\n" * 20}])
    assert bloat_flag([{"added_lines": 200, "removed_lines": 0}]) is not None
    assert bloat_flag([{"added_lines": 5, "removed_lines": 0}]) is None
    assert redaction_violations("see internal/foo", []) == ["internal/"]
    assert redaction_violations("acme rocks", ["acme"]) == ["project:acme"]
    assert redaction_violations("fnord is fine", ["acme"]) == []
    print("guards ok")
