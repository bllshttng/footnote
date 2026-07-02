"""Boundary-reconcile detection (x-d0ad).

When ``/target`` picks up a node whose plan/brief was written BEFORE a done
blocker's PR merged, the fresh-context worker builds on stale assumptions. This
computes, per done blocker, a mechanical staleness verdict for the orientation
report:

  reconciled  -- the plan already carries the blocker's landed-marker (the
                 resume/handoff case). Checked FIRST, per blocker.
  fresh       -- the plan/brief file mtime is newer than the blocker's
                 ``completed_at`` AND the doc carries no reconcile markers yet.
                 Once Step 0 has appended any marker, mtime is a poisoned proxy
                 (the append bumped it), so an un-markered blocker reads *stale*
                 instead -- that is what stops an A-reconcile append from masking
                 a different blocker B that merged later.
  stale       -- neither: the /target spine's Step 0 must read the blocker's
                 landed diff and append a section before the first code commit.
  unknown     -- detection failed for this entry (bad graph/stat/pr). Rendered,
                 never raised -- ``fno target init`` must not crash on it.

Detection is advisory here (guidelines-not-gates, matching ``reconcile.py``);
the ``/target`` spine step is what makes acting on a STALE verdict mandatory.

Distinct from the ``--reconcile <manifest>`` de-stub mode: that serves contract
dependents that stubbed against an unlanded interface. Boundary reconcile serves
hard-serialized dependents that never stubbed anything.

ponytail: marker-grep + one mtime stat is the whole check. No sidecar state --
the appended plan section IS the durable marker.
"""
from __future__ import annotations

import re
from dataclasses import dataclass, replace
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional


@dataclass(frozen=True)
class BlockerVerdict:
    blocker_id: str
    verdict: str  # "stale" | "fresh" | "reconciled" | "unknown"
    pr_number: Optional[int] = None
    completed_at: Optional[str] = None
    reason: Optional[str] = None  # populated for "unknown"


def _brief_path(node_id: str) -> Path:
    """Convention path for a node's sidecar brief. Overridable in tests."""
    return Path.home() / ".fno" / "briefs" / f"{node_id}.md"


def _resolve_target(node: dict, plan_or_brief_path: Optional[str]) -> Optional[Path]:
    """The file whose mtime/marker we check: ``plan_path`` (``#anchor`` stripped)
    if it exists, else the node's brief when ``has_brief``, else None."""
    if plan_or_brief_path:
        p = Path(str(plan_or_brief_path).split("#", 1)[0])
        if p.exists():
            return p
    nid = node.get("id")
    if node.get("has_brief") and nid:
        bp = _brief_path(str(nid))
        if bp.exists():
            return bp
    return None


def _find(graph: list, token: str) -> Optional[dict]:
    """Graph entry matching ``token`` by id OR slug (format-agnostic, cheap)."""
    low = str(token).lower()
    for e in graph:
        if not isinstance(e, dict):
            continue
        if str(e.get("id", "")).lower() == low or str(e.get("slug", "")).lower() == low:
            return e
    return None


def _reconcile_against(text: str) -> list[str]:
    """The plan frontmatter's ``reconcile_against:`` list (additive escape hatch
    for deps not modeled as blockers). Empty on any parse failure."""
    try:
        import yaml

        from fno.plan._doc import _split_frontmatter

        fm, _ = _split_frontmatter(text)
        data = yaml.safe_load(fm) if fm.strip() else None
        val = (data or {}).get("reconcile_against") if isinstance(data, dict) else None
        if isinstance(val, str):
            return [val]
        if isinstance(val, list):
            return [str(v) for v in val if v]
    except Exception:  # noqa: BLE001 - a bad frontmatter is not a detection failure
        pass
    return []


def _is_marker_heading(s: str) -> bool:
    return s.startswith("#") and "landed" in s.lower()


def _has_any_marker(text: str) -> bool:
    """True if the doc carries ANY ``### ... landed ...`` heading -- i.e. Step 0
    has run at least once. Once it has, the file mtime is a poisoned freshness
    proxy (the reconcile append bumped it), so un-markered blockers must not be
    trusted as fresh on mtime alone."""
    return any(_is_marker_heading(line.strip()) for line in text.splitlines())


def _marker_present(text: str, blocker_id: str, pr_number: Optional[int]) -> bool:
    """True if a ``### <blocker-id> landed ...`` heading (id OR PR-number match)
    is already in the doc -- the idempotence marker Step 0 writes. Both keys match
    on a TOKEN boundary so ``x-14`` does not match ``x-141`` and ``#2`` does not
    match ``#20``."""
    # (?<![\w-]) / (?!\w) so a hyphenated id is matched whole, not as a prefix.
    id_re = re.compile(rf"(?<![\w-]){re.escape(blocker_id)}(?![\w-])", re.IGNORECASE)
    pr_re = re.compile(rf"#{pr_number}(?!\d)") if pr_number is not None else None
    for line in text.splitlines():
        s = line.strip()
        if not _is_marker_heading(s):
            continue
        if id_re.search(s):
            return True
        if pr_re is not None and pr_re.search(s):
            return True
    return False


def _is_fresh(completed_at: str, mtime: float) -> bool:
    """True when the file was edited AFTER the blocker merged. Raises on an
    unparseable timestamp (caught by the caller -> unknown verdict)."""
    dt = datetime.fromisoformat(str(completed_at).replace("Z", "+00:00"))
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return mtime > dt.timestamp()


def _evaluate(
    entry: Optional[dict], explicit: bool, text: str, mtime: float, doc_markered: bool
) -> Optional[BlockerVerdict]:
    """Verdict for one blocker, or None to skip it silently. ``explicit`` marks a
    ``reconcile_against:`` entry -- those surface as ``unknown`` instead of a
    silent skip so a mis-listed id is visible."""
    if entry is None:
        return BlockerVerdict("(unknown)", "unknown", reason="not in graph") if explicit else None
    bid = str(entry.get("id") or "(unknown)")
    status = str(entry.get("_status") or entry.get("status") or "").strip()
    if status != "done":
        # Open blocker means the node should not have dispatched; a done-but-not
        # -this blocker is simply not landed. Silent skip for graph blockers.
        return BlockerVerdict(bid, "unknown", reason=f"not done ({status})") if explicit else None
    pr = entry.get("pr_number")
    completed = entry.get("completed_at")
    if pr is None:
        # Done doc/DoneAdvisory node: no diff to read. Surface only when the user
        # explicitly asked for it; artifact-diff reading is deferred (v1).
        return (
            BlockerVerdict(bid, "unknown", completed_at=completed, reason="done blocker without PR")
            if explicit
            else None
        )
    if _marker_present(text, bid, pr):
        return BlockerVerdict(bid, "reconciled", pr_number=pr, completed_at=completed)
    if not completed:
        return BlockerVerdict(bid, "unknown", pr_number=pr, reason="no completed_at")
    try:
        fresh = _is_fresh(completed, mtime)
    except (ValueError, TypeError):
        return BlockerVerdict(bid, "unknown", pr_number=pr, completed_at=completed, reason="bad completed_at")
    # mtime-fresh is trustworthy ONLY on an un-touched doc. Once Step 0 has
    # appended any marker (doc_markered), that append bumped the file mtime past
    # this un-markered blocker's completed_at, which would false-fresh it and mask
    # real drift. Bias to stale (a wasted diff read) over masking (the bad way).
    verdict = "fresh" if (fresh and not doc_markered) else "stale"
    return BlockerVerdict(bid, verdict, pr_number=pr, completed_at=completed)


def boundary_reconcile(
    node: dict, plan_or_brief_path: Optional[str], graph: list
) -> list[BlockerVerdict]:
    """Per-blocker staleness verdicts for the orientation report.

    Reads the node's ``blocked_by`` plus any ``reconcile_against:`` in the
    plan/brief frontmatter, and for each *done* blocker classifies the plan as
    reconciled / fresh / stale (or unknown on a read failure). Returns ``[]``
    when there is no plan/brief to check (a bare-idea ``/target`` reads landed
    code by construction). Never raises -- a total failure degrades to a single
    ``unknown`` verdict so init still prints its line.
    """
    try:
        target = _resolve_target(node, plan_or_brief_path)
        if target is None:
            return []
        try:
            text = target.read_text(encoding="utf-8")
            mtime = target.stat().st_mtime
        except (OSError, UnicodeDecodeError) as exc:
            blocked = node.get("blocked_by") or []
            return [BlockerVerdict(str(b), "unknown", reason=f"plan unreadable: {exc}") for b in blocked]

        pairs: list[tuple[str, bool]] = [(str(b), False) for b in (node.get("blocked_by") or [])]
        seen = {t.lower() for t, _ in pairs}
        for tok in _reconcile_against(text):
            if tok.lower() not in seen:
                pairs.append((tok, True))
                seen.add(tok.lower())

        doc_markered = _has_any_marker(text)
        out: list[BlockerVerdict] = []
        for token, explicit in pairs:
            try:
                v = _evaluate(_find(graph, token), explicit, text, mtime, doc_markered)
            except Exception as exc:  # noqa: BLE001 - one bad blocker never sinks the rest
                v = BlockerVerdict(token, "unknown", reason=str(exc))
            if v is not None:
                # carry the input token when the entry could not name itself
                if v.blocker_id == "(unknown)":
                    v = replace(v, blocker_id=token)
                out.append(v)
        return out
    except Exception as exc:  # noqa: BLE001 - AC8-FR: detection never crashes init
        return [BlockerVerdict("(detection)", "unknown", reason=str(exc))]


def _self_check() -> None:
    import os
    import tempfile

    with tempfile.TemporaryDirectory() as d:
        plan = Path(d) / "plan.md"
        graph = [
            {"id": "x-aaaa", "_status": "done", "pr_number": 11, "completed_at": "2026-07-02T00:00:00+00:00"},
        ]
        node = {"id": "x-self", "blocked_by": ["x-aaaa"]}

        # stale: plan older than the blocker's merge
        plan.write_text("# plan\n", encoding="utf-8")
        os.utime(plan, (0, 0))  # epoch 0 -> older than 2026
        v = boundary_reconcile(node, str(plan), graph)
        assert len(v) == 1 and v[0].verdict == "stale", v

        # reconciled: marker present wins over mtime
        plan.write_text("# plan\n### x-aaaa landed (PR #11)\n", encoding="utf-8")
        os.utime(plan, (0, 0))
        v = boundary_reconcile(node, str(plan), graph)
        assert v[0].verdict == "reconciled", v

        # fresh: plan newer than the merge (mtime = now)
        plan.write_text("# plan\n", encoding="utf-8")
        v = boundary_reconcile(node, str(plan), graph)
        assert v[0].verdict == "fresh", v

        # no plan, no brief -> empty
        assert boundary_reconcile({"id": "x-none"}, None, graph) == []
    print("boundary self-check OK")


if __name__ == "__main__":
    _self_check()
