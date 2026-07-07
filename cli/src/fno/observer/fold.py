"""Pure fold: corpus construction + scoring for the observer harness (x-57a5).

Read-only over already-captured signals (ledger, graph, events, postmortems) -
corpus construction is a read-time fold, never a new write-path for historical
facts (Locked Decision 1). Skill-invocation attribution and version resolution
are reused verbatim from ``fno.scoreboard.fold`` (Locked Decision 7) - this
module does not re-derive them.

I/O (plan-file reads, gh PR-thread fetches) stays at the CLI layer; the
functions here take pre-fetched inputs so they are unit-testable without
touching disk or the network, mirroring ``scoreboard/fold.py``'s style.
"""

from __future__ import annotations

import re
from collections import Counter
from datetime import datetime, timedelta
from typing import Optional

import yaml

from fno.scoreboard.fold import (
    _default_read_transcript,
    _default_skill_version,
    _extract_skill_runs,
    _is_shipped_reason,
    _node_outcome,
    _parse_ts,
    _pct,
)

# Mirrors _CALIBRATION_MIN_VERDICTS (scoreboard/fold.py): below this, a ranking
# is fabricated confidence, not a real trend (Locked Decision 11).
MIN_ATTRIBUTABLE = 10

_SKILL_IDS = {"blueprint": "fno:blueprint", "review": "fno:review"}

# Claude's Discretion item 6: postmortem blocked_reason.kind -> attribution
# class. `None` (unclassified) is a real third state, not an error - a kind
# outside both sets stays unclassified rather than forcing a guess.
_PLAN_ATTRIBUTABLE_KINDS = frozenset(
    {"missing_dependency", "architecture_decision", "ambiguous_requirement"}
)
_EXECUTION_ATTRIBUTABLE_KINDS = frozenset({"test_failure", "tool_error", "environment"})


def classify_postmortem(kind: Optional[str]) -> Optional[str]:
    """plan-attributable | execution-attributable | None (unclassified/no kind)."""
    if kind in _PLAN_ATTRIBUTABLE_KINDS:
        return "plan-attributable"
    if kind in _EXECUTION_ATTRIBUTABLE_KINDS:
        return "execution-attributable"
    return None


def _first_session_id(row: dict) -> Optional[str]:
    sessions = row.get("sessions")
    if isinstance(sessions, list) and sessions:
        first = sessions[0]
        return first if isinstance(first, str) else None
    return None


def build_corpus(
    rows: list[dict],
    graph_nodes: list[dict],
    postmortems: list[dict],
    *,
    skill: str,
    since_days: int,
    now: datetime,
    read_transcript=None,
    resolve_skill_version=None,
) -> dict:
    """Read-only fold: ledger rows attributed to *skill* -> corpus items.

    ``postmortems`` is a pre-read list of ``{session_id, graph_node_id,
    blocked_reason_kind}`` dicts (I/O done by the caller). A row that ran a
    different skill is simply not attributed here - never a crash, never a
    counted gap (only a genuine fetch/resolution failure for an attributed
    item counts against coverage, tracked by the caller).

    Returns ``{"items": [...], "total_rows": int, "attributed": int}``.
    """
    skill_id = _SKILL_IDS.get(skill, f"fno:{skill}")
    cutoff = now - timedelta(days=since_days)

    def _in_window(ts_raw) -> bool:
        dt = _parse_ts(ts_raw)
        return dt is not None and cutoff <= dt <= now

    windowed = [r for r in rows if _in_window(r.get("completed"))]
    read_transcript = read_transcript or _default_read_transcript
    resolve_skill_version = resolve_skill_version or _default_skill_version

    by_id = {n.get("id"): n for n in graph_nodes if n.get("id")}
    fixes: dict[str, list[dict]] = {}
    for n in graph_nodes:
        origin = n.get("caused_by")
        if origin:
            fixes.setdefault(origin, []).append(n)
    w4_available = any(("reverted" in n) or n.get("caused_by") for n in graph_nodes)

    pm_by_node: dict[str, str] = {}
    pm_by_session: dict[str, str] = {}
    for pm in postmortems:
        kind = pm.get("blocked_reason_kind")
        if not kind:
            continue
        if pm.get("graph_node_id"):
            pm_by_node[pm["graph_node_id"]] = kind
        if pm.get("session_id"):
            pm_by_session[pm["session_id"]] = kind

    items: list[dict] = []
    for r in windowed:
        skills, method = _extract_skill_runs(r, read_transcript=read_transcript)
        if skill_id not in skills:
            continue

        nid = r.get("graph_node_id")
        session_id = _first_session_id(r)
        shipped = _is_shipped_reason(r.get("termination_reason"))
        judgeable = bool(shipped and nid and w4_available and nid in by_id)
        outcome = (
            _node_outcome(nid, _parse_ts(r.get("completed")), by_id, fixes)
            if judgeable
            else None
        )
        version = resolve_skill_version(skill_id, r.get("completed"))
        kind = (nid and pm_by_node.get(nid)) or (session_id and pm_by_session.get(session_id))
        items.append(
            {
                "session_id": session_id,
                "graph_node_id": nid,
                "plan_path": r.get("plan_path"),
                "skill_id": skill_id,
                "skill_version": version,
                "method": method,
                "shipped": shipped,
                "termination_reason": r.get("termination_reason"),
                "judgeable": judgeable,
                "outcome": outcome,
                "attribution_class": classify_postmortem(kind),
            }
        )

    return {"items": items, "total_rows": len(windowed), "attributed": len(items)}


# -- Structural checks (blueprint dimension), reused-in-spirit from
# skills/blueprint/mutate_doc.py's hard-refuse check and
# skills/do/orchestrator.py's detect_hidden_output_conflicts. Reimplemented
# in pure Python rather than imported: cli/src/fno lint (shellout-drift)
# forbids a packaged verb shelling to a repo-root script, and those two
# checks live in skill scripts outside the installable package.

_FAILURE_MODES_RE = re.compile(r"^## Failure Modes\s*$", re.MULTILINE)
_EXEC_STRATEGY_FENCE_RE = re.compile(
    r"## Execution Strategy\s*\n```ya?ml\n(.*?)\n```", re.DOTALL
)


def has_failure_modes_heading(plan_text: str) -> bool:
    """The literal check /blueprint hard-refuses without (mutate_doc.py)."""
    return bool(_FAILURE_MODES_RE.search(plan_text))


def find_file_ownership_collisions(plan_text: str) -> list[str]:
    """Files claimed by more than one task's ``surface`` list in the plan's
    Execution Strategy block. Missing/unparseable block -> no collisions
    detectable, not an error (the plan is simply not judgeable on this
    dimension by this best-effort check)."""
    m = _EXEC_STRATEGY_FENCE_RE.search(plan_text)
    if not m:
        return []
    try:
        strategy = yaml.safe_load(m.group(1))
    except yaml.YAMLError:
        return []
    if not isinstance(strategy, dict):
        return []
    tasks = strategy.get("tasks")
    if not isinstance(tasks, list):
        return []
    owners: dict[str, int] = Counter()
    for t in tasks:
        if not isinstance(t, dict):
            continue
        surface = t.get("surface")
        if isinstance(surface, list):
            for f in surface:
                if isinstance(f, str):
                    owners[f] += 1
    return sorted(f for f, n in owners.items() if n > 1)


def score_blueprint_item(item: dict, *, plan_text: Optional[str]) -> dict[str, Optional[str]]:
    """Score one corpus item on blueprint's dimensions.

    ``shipped_outcome`` is omitted from the result for a replay item (caller
    passes ``item`` without ``judgeable``/``outcome`` populated meaningfully
    for a fresh, never-built replay plan - Review Amendment A1: replay scores
    structural dimensions only). ``None`` for a dimension means "not
    scorable" (counted as a coverage gap by the caller), never a fabricated
    verdict.
    """
    result: dict[str, Optional[str]] = {}

    if plan_text is None:
        result["structural_validity"] = None
        result["collision_free"] = None
    else:
        result["structural_validity"] = "pass" if has_failure_modes_heading(plan_text) else "fail"
        collisions = find_file_ownership_collisions(plan_text)
        result["collision_free"] = "pass" if not collisions else "fail"

    if item.get("include_shipped_outcome", True):
        outcome = item.get("outcome")
        if not item.get("judgeable"):
            result["shipped_outcome"] = None
        elif outcome == "merged_clean":
            result["shipped_outcome"] = "pass"
        elif outcome in ("bounced", "reverted"):
            attribution = item.get("attribution_class")
            result["shipped_outcome"] = "fail" if attribution == "plan-attributable" else "degraded"
        else:
            result["shipped_outcome"] = None

    return result


def score_review_item(
    *, addressed_ids: set[str], skipped_ids: set[str], all_finding_ids: set[str]
) -> dict[str, Optional[str]]:
    """Score one review corpus item on finding_precision (Locked Decision 9).

    pass: every raised finding was either fixed or explicitly logged as
    skipped (a documented disposition). fail: at least one finding was
    neither addressed nor logged - declined and never contradicted. degraded:
    a mix. No findings at all is not scorable (a review with nothing to
    say has no precision to measure) -> None, a coverage gap.
    """
    if not all_finding_ids:
        return {"finding_precision": None}
    dispositioned = addressed_ids | skipped_ids
    unresolved = all_finding_ids - dispositioned
    if not unresolved:
        verdict = "pass"
    elif len(unresolved) < len(all_finding_ids):
        verdict = "degraded"
    else:
        verdict = "fail"
    return {"finding_precision": verdict}


def build_run_summary(
    *,
    run_id: str,
    skill_id: str,
    skill_version: str,
    findings: list[tuple[str, str]],
    corpus_size: int,
    scored_count: int,
    skill_ref: Optional[str] = None,
    require_min: bool = True,
) -> dict:
    """Aggregate a run's (dimension, verdict) findings into the
    ``skill_eval_run_complete`` payload shape, or an ``insufficient`` state.

    ``findings`` must already exclude None (unscorable) entries - each is one
    real, dimension-scoped verdict. ``corpus_size`` gates on
    :data:`MIN_ATTRIBUTABLE` (Locked Decision 11); ``scored_count`` (<=
    corpus_size) drives ``coverage_pct``, the honesty rule carried over from
    ``fno scoreboard``.

    The <10 gate is a *sweep* rule (a retrospective ranking under 10 attributable
    runs is fabricated confidence). A *replay* batch is a targeted before/after on
    specific items, not a trend, so it passes ``require_min=False`` and reports
    its true, small ``corpus_size`` rather than a padded one (a HEAD replay has no
    ``skill_ref``, so the flag - not ``skill_ref`` presence - controls the gate).
    """
    if require_min and corpus_size < MIN_ATTRIBUTABLE:
        return {"state": "insufficient", "need": MIN_ATTRIBUTABLE, "n": corpus_size}

    counts = Counter(v for _d, v in findings)
    fail_by_dim: Counter = Counter()
    for d, v in findings:
        if v == "fail":
            fail_by_dim[d] += 1
    failure_ranking = [
        {"dimension": d, "fail_count": n}
        for d, n in sorted(fail_by_dim.items(), key=lambda kv: (-kv[1], kv[0]))
    ]

    out = {
        "state": "ok",
        "run_id": run_id,
        "skill_id": skill_id,
        "skill_version": skill_version,
        "corpus_size": corpus_size,
        "pass_count": counts.get("pass", 0),
        "degraded_count": counts.get("degraded", 0),
        "fail_count": counts.get("fail", 0),
        "coverage_pct": _pct(scored_count, corpus_size),
        "failure_ranking": failure_ranking,
    }
    if skill_ref is not None:
        out["skill_ref"] = skill_ref
    return out


if __name__ == "__main__":
    # ponytail self-check: the fold's load-bearing invariants, no framework.
    now = datetime(2026, 7, 4, 12, 0, 0)

    # classify_postmortem
    assert classify_postmortem("missing_dependency") == "plan-attributable"
    assert classify_postmortem("test_failure") == "execution-attributable"
    assert classify_postmortem("something_new") is None
    assert classify_postmortem(None) is None

    # build_corpus: attribution + outcome join
    rows = [
        {
            "completed": "2026-07-01T10:00:00",
            "termination_reason": "DonePRGreen",
            "graph_node_id": "x-1",
            "sessions": ["s1"],
            "phases_completed": ["plan"],
        },
        {
            "completed": "2026-07-02T10:00:00",
            "termination_reason": "DonePRGreen",
            "graph_node_id": "x-2",
            "sessions": ["s2"],
            "phases_completed": ["do"],  # different skill -> not attributed
        },
    ]
    graph_nodes = [{"id": "x-1", "reverted": False}, {"id": "x-2", "reverted": False}]
    corpus = build_corpus(rows, graph_nodes, [], skill="blueprint", since_days=28, now=now)
    assert corpus["total_rows"] == 2, corpus
    assert corpus["attributed"] == 1, corpus
    assert corpus["items"][0]["outcome"] == "merged_clean", corpus

    # insufficient guard
    summary = build_run_summary(
        run_id="obs-x", skill_id="fno:blueprint", skill_version="unknown",
        findings=[("structural_validity", "pass")], corpus_size=3, scored_count=3,
    )
    assert summary == {"state": "insufficient", "need": 10, "n": 3}, summary

    # ok state: ranking + coverage
    findings = [
        ("structural_validity", "pass"),
        ("structural_validity", "fail"),
        ("collision_free", "fail"),
        ("shipped_outcome", "degraded"),
    ]
    summary = build_run_summary(
        run_id="obs-fno:blueprint-x", skill_id="fno:blueprint", skill_version="abc1234",
        findings=findings, corpus_size=12, scored_count=10,
    )
    assert summary["state"] == "ok", summary
    assert summary["fail_count"] == 2 and summary["degraded_count"] == 1 and summary["pass_count"] == 1, summary
    assert summary["coverage_pct"] == 83, summary  # round(100*10/12)
    assert summary["failure_ranking"][0]["fail_count"] == 1, summary  # both dims tied at 1

    # structural checks
    text_missing = "# Plan\n\n## Overview\nno failure modes here\n"
    assert has_failure_modes_heading(text_missing) is False
    text_ok = "# Plan\n\n## Failure Modes\n\nstuff\n"
    assert has_failure_modes_heading(text_ok) is True

    strategy_text = (
        "## Execution Strategy\n\n```yaml\n"
        "tasks:\n- id: '1.1'\n  surface: ['a.py', 'b.py']\n"
        "- id: '1.2'\n  surface: ['b.py', 'c.py']\n"
        "```\n"
    )
    assert find_file_ownership_collisions(strategy_text) == ["b.py"], find_file_ownership_collisions(strategy_text)

    # score_blueprint_item: bounced + plan-attributable -> fail; execution -> degraded
    item = {"judgeable": True, "outcome": "bounced", "attribution_class": "plan-attributable"}
    assert score_blueprint_item(item, plan_text=text_ok)["shipped_outcome"] == "fail"
    item2 = {"judgeable": True, "outcome": "bounced", "attribution_class": "execution-attributable"}
    assert score_blueprint_item(item2, plan_text=text_ok)["shipped_outcome"] == "degraded"
    item3 = {"judgeable": False, "outcome": None}
    assert score_blueprint_item(item3, plan_text=None)["shipped_outcome"] is None
    assert score_blueprint_item(item3, plan_text=None)["structural_validity"] is None

    # score_review_item
    assert score_review_item(addressed_ids=set(), skipped_ids=set(), all_finding_ids=set()) == {
        "finding_precision": None
    }
    assert score_review_item(
        addressed_ids={"c1"}, skipped_ids={"c2"}, all_finding_ids={"c1", "c2"}
    ) == {"finding_precision": "pass"}
    assert score_review_item(
        addressed_ids=set(), skipped_ids=set(), all_finding_ids={"c1", "c2"}
    ) == {"finding_precision": "fail"}
    assert score_review_item(
        addressed_ids={"c1"}, skipped_ids=set(), all_finding_ids={"c1", "c2"}
    ) == {"finding_precision": "degraded"}

    print("ok")
