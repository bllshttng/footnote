"""Pure event-processing core for the skill-diff proposer.

Everything here takes an already-read event list (or a path) and returns data -
no synthesis, no PR, no git. That keeps the whole decision surface (which run to
process, what findings apply, has the local-maxima ceiling tripped, what the PR
body says) unit-testable without an LLM or a network.
"""
from __future__ import annotations

import json
import logging
from pathlib import Path
from typing import Optional

_LOG = logging.getLogger(__name__)

LOOP_NAME = "skill_diff_proposer"

# Terminal records that mark a run_id as already handled (idempotency key is
# (run_id, skill_id)). skill_eval_run_complete is the
# trigger; these three are the proposer's own outcomes.
_TERMINAL_TYPES = ("skill_diff_proposed", "skill_diff_no_diff_helps", "skill_diff_noop")

# Local-maxima ceiling: how many consecutive run_complete cycles a skill's top
# failure dimension may stay unchanged (with >=1 proposal in between) before the
# proposer stops diffing that dimension and files a node (AC7-EDGE).
LOCAL_MAXIMA_WINDOW = 3


def read_events_tolerant(path: Path) -> list[dict]:
    """Read events.jsonl skipping corrupt lines (AC3-ERR).

    The shared ``read_events`` raises on the first bad line; a standing loop
    tick must never crash on one malformed append (mirrors fold.py's tolerant
    readers). A skipped line is logged, not silently swallowed.
    """
    path = Path(path)
    if not path.exists():
        return []
    out: list[dict] = []
    for lineno, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        raw = raw.strip()
        if not raw:
            continue
        try:
            out.append(json.loads(raw))
        except json.JSONDecodeError as exc:
            _LOG.warning("skill-diff: skipping corrupt events.jsonl line %d: %s", lineno, exc)
    return out


def _data(event: dict) -> dict:
    return event.get("data") or {}


def unprocessed_runs(events: list[dict], skill_id: str) -> list[str]:
    """run_ids with a run_complete for *skill_id* but no terminal proposer record.

    Returned oldest-first (append order of the run_complete event). The
    idempotency invariant (AC8-FR): a run_id that already produced a proposal /
    node / noop is never reprocessed, so a second concurrent tick is a no-op.
    """
    handled: set[str] = set()
    complete: list[str] = []
    for e in events:
        t = e.get("type")
        d = _data(e)
        if t in _TERMINAL_TYPES:
            rid = d.get("run_id")
            if rid:
                handled.add(rid)
        elif t == "skill_eval_run_complete" and d.get("skill_id") == skill_id:
            rid = d.get("run_id")
            if rid and rid not in complete:
                complete.append(rid)
    return [rid for rid in complete if rid not in handled]


def run_complete_event(events: list[dict], run_id: str) -> Optional[dict]:
    for e in events:
        if e.get("type") == "skill_eval_run_complete" and _data(e).get("run_id") == run_id:
            return e
    return None


def findings_for_run(events: list[dict], run_id: str) -> list[dict]:
    """skill_eval_finding data dicts for *run_id*, excluding tool faults.

    A tool_fault=true finding is a replay-harness crash, not a skill-quality
    verdict - the schema requires downstream to exclude it from failure
    rankings, so a spawn timeout never masquerades as a skill that needs fixing.
    """
    out = []
    for e in events:
        if e.get("type") != "skill_eval_finding":
            continue
        d = _data(e)
        if d.get("run_id") != run_id:
            continue
        if d.get("tool_fault") is True:
            continue
        out.append(d)
    return out


def failure_ranking(events: list[dict], run_id: str) -> list[dict]:
    """[{dimension, fail_count}] descending. Prefer the run_complete's own
    ranking (the observer already excluded coverage gaps); fall back to counting
    non-tool-fault fail findings when the terminal event lacks a ranking."""
    rc = run_complete_event(events, run_id)
    if rc:
        ranking = _data(rc).get("failure_ranking")
        if ranking:
            return list(ranking)
    counts: dict[str, int] = {}
    for f in findings_for_run(events, run_id):
        if f.get("verdict") == "fail":
            counts[f["dimension"]] = counts.get(f["dimension"], 0) + 1
    return [
        {"dimension": dim, "fail_count": n}
        for dim, n in sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))
    ]


def has_actionable_findings(events: list[dict], run_id: str) -> bool:
    """True when at least one non-tool-fault finding is fail or degraded (AC6-EDGE).

    A run whose findings are all ``pass`` is a no-op: there is nothing to fix,
    and a proposer that "always does something" would fabricate a diff.
    """
    return any(f.get("verdict") in ("fail", "degraded") for f in findings_for_run(events, run_id))


def top_dimension(events: list[dict], run_id: str) -> Optional[str]:
    ranking = failure_ranking(events, run_id)
    return ranking[0]["dimension"] if ranking else None


def local_maxima_tripped(events: list[dict], skill_id: str, run_id: str) -> bool:
    """The top failure dimension has resisted proposals across the window (AC7-EDGE).

    Mechanical, not judged: if the last ``LOCAL_MAXIMA_WINDOW``
    run_complete cycles for this skill (including the current run) all share the
    same top failure dimension, AND at least one ``skill_diff_proposed`` landed
    for this skill somewhere in that span, then diffing that dimension again is
    chasing a local maximum - file a node instead.

    ponytail: "at least one proposal in the span" stands in for "at least one
    MERGED proposal". Merge status needs a gh round-trip per PR and offline it
    is unknowable; a proposal that never merged still means the loop tried and
    the dimension stayed on top. Upgrade to merge-aware if false ceilings show
    up in practice.
    """
    current_top = top_dimension(events, run_id)
    if current_top is None:
        return False

    # Ordered run_complete run_ids for this skill, up to and including run_id.
    seq: list[str] = []
    for e in events:
        if e.get("type") == "skill_eval_run_complete" and _data(e).get("skill_id") == skill_id:
            rid = _data(e).get("run_id")
            if rid and rid not in seq:
                seq.append(rid)
            if rid == run_id:
                break
    window = seq[-LOCAL_MAXIMA_WINDOW:]
    if len(window) < LOCAL_MAXIMA_WINDOW:
        return False
    if any(top_dimension(events, rid) != current_top for rid in window):
        return False
    proposed_in_span = any(
        e.get("type") == "skill_diff_proposed"
        and _data(e).get("skill_id") == skill_id
        and _data(e).get("run_id") in window
        for e in events
    )
    return proposed_in_span


def prior_proposed(events: list[dict], skill_id: str) -> list[dict]:
    """This skill's skill_diff_proposed data dicts, append order (bloat input)."""
    return [
        _data(e)
        for e in events
        if e.get("type") == "skill_diff_proposed" and _data(e).get("skill_id") == skill_id
    ]


def build_pr_body(
    *,
    run_id: str,
    skill_id: str,
    hunks: list[dict],
    justification: Optional[str],
    bloat: Optional[dict],
    version_observed: str,
    version_against: str,
    is_review_skill: bool,
) -> str:
    """Assemble the PR body: one section per hunk naming its cited finding IDs,
    the justification section (if additive-only), the bloat flag (if tripped),
    and both skill hashes (AC9-FR). Pattern-level prose only - the redaction
    guard runs over the returned text before the PR opens (A3)."""
    lines = [
        f"## Skill-diff proposal: `{skill_id}`",
        "",
        f"Synthesized from observer run `{run_id}`. Every hunk below cites the "
        "observed failure it responds to; a reviewer judges the evidence the "
        "same way they would any PR description.",
        "",
        f"- skill version observed by the sweep: `{version_observed}`",
        f"- skill version this diff is against: `{version_against}`",
    ]
    if version_observed != version_against:
        lines.append(
            "- note: the skill file moved between eval and proposal; the diff is "
            "computed against current content (AC9-FR)."
        )
    lines.append("")

    if bloat:
        lines += [
            f"> **bloat_review_needed**: this skill's last {bloat['window']} proposals "
            f"net +{bloat['net_growth']} lines (threshold {bloat['threshold']}). Flagged, "
            "not blocked - the merge gate is the control point.",
            "",
        ]

    if justification:
        lines += ["### Justification (additive-only diff)", "", justification, ""]

    if is_review_skill:
        lines += [
            "> Replay v1 covers /blueprint only. This /review diff is validated by "
            "sweep-mode evidence and post-merge sweeps, not a pre-merge replay "
            "comparison.",
            "",
        ]

    lines.append("### Cited hunks")
    lines.append("")
    for i, h in enumerate(hunks, 1):
        cites = ", ".join(h.get("cited_finding_ids") or [])
        lines.append(f"**{i}. `{h.get('file', '?')}`** — addresses: {cites}")
        rationale = (h.get("rationale") or "").strip()
        if rationale:
            lines.append(f"  {rationale}")
        lines.append("")

    lines += [
        "---",
        "_Structurally motivated by replay/sweep evidence; outcome confirmation "
        "accrues from post-merge observer sweeps. A human "
        "merges - this loop is assisted, never unattended._",
    ]
    return "\n".join(lines)


if __name__ == "__main__":  # pragma: no cover - smoke self-check
    evs = [
        {"type": "skill_eval_run_complete", "data": {"run_id": "r1", "skill_id": "fno:blueprint",
         "failure_ranking": [{"dimension": "structural_validity", "fail_count": 2}]}},
        {"type": "skill_eval_finding", "data": {"run_id": "r1", "skill_id": "fno:blueprint",
         "dimension": "structural_validity", "verdict": "fail"}},
        {"type": "skill_eval_finding", "data": {"run_id": "r1", "skill_id": "fno:blueprint",
         "dimension": "structural_validity", "verdict": "fail", "tool_fault": True}},
    ]
    assert unprocessed_runs(evs, "fno:blueprint") == ["r1"]
    assert len(findings_for_run(evs, "r1")) == 1  # tool_fault excluded
    assert has_actionable_findings(evs, "r1")
    assert top_dimension(evs, "r1") == "structural_validity"
    evs.append({"type": "skill_diff_noop", "data": {"run_id": "r1", "skill_id": "fno:blueprint"}})
    assert unprocessed_runs(evs, "fno:blueprint") == []  # now handled
    print("engine ok")
