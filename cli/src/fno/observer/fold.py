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
    _WEDGE_REASONS,
    _ci_reds_from_fires,
    _default_read_transcript,
    _default_skill_version,
    _extract_skill_runs,
    _is_shipped_reason,
    _node_outcome,
    _parse_ts,
    _pct,
    _row_session_ids,
    _transcript_counts,
)

# Mirrors _CALIBRATION_MIN_VERDICTS (scoreboard/fold.py): below this, a ranking
# is fabricated confidence, not a real trend (Locked Decision 11).
MIN_ATTRIBUTABLE = 10

_SKILL_IDS = {"blueprint": "fno:blueprint", "review": "fno:review", "target": "fno:target"}

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


# --------------------------------------------------------------------------- #
# target: PR-anchored corpus (x-6ff0)
#
# The denominator is real merged+closed PRs, walked BACK to node -> sessions,
# never the ledger (which drops ~52% of merged PRs and so is a *process* record,
# not a *delivery* one). Every reused primitive - _node_outcome, the w4 gate,
# _ci_reds_from_fires, _row_session_ids, _WEDGE_REASONS - is imported, never
# reimplemented (Locked Decisions 1, 6).
# --------------------------------------------------------------------------- #

_PR_URL_REPO_RE = re.compile(r"github\.com/([^/]+/[^/]+)/pull/")

# no-PR class terminals: a run that reached one of these STOPPED without shipping
# anything (the churny / abandoned / crash-terminated attempts the 84%-green stat
# silently dropped). The discriminator is the terminal reason, NOT the phase set:
# a NoProgress that wedged during planning carries phases_completed=[think, plan]
# yet is a failed attempt, not a clean plan thread - keying on phases would
# wrongly exclude it. Ship terminals, `None` (incomplete/unknown), `delegated`
# (handed off, not waste) and `NoWork` (loop found nothing) are all excluded.
_NO_PR_TERMINALS = _WEDGE_REASONS | {"Interrupted"}


def _repo_from_url(url: Optional[str]) -> Optional[str]:
    """`https://github.com/owner/name/pull/149` -> `owner/name`, else None."""
    if not url:
        return None
    m = _PR_URL_REPO_RE.search(url)
    return m.group(1) if m else None


def _resolve_pr_node(pr: dict, by_id: dict, by_pr_url: dict, by_pr_num_repo: dict) -> Optional[dict]:
    """PR -> graph node, walking back. Branch `feature/<id>` first (cheap), then
    the reverse graph lookup by exact pr_url, then (pr_number, repo) - branch
    name alone re-creates the loss on squash-merges that drop the branch (Domain
    Pitfall). None -> the PR is unattributable (a counted coverage gap, never a
    silent drop)."""
    head = pr.get("headRefName") or ""
    if head.startswith("feature/"):
        node = by_id.get(head[len("feature/"):])
        if node is not None:
            return node
    url = pr.get("url")
    node = by_pr_url.get(url)
    if node is not None:
        return node
    repo = _repo_from_url(url)
    number = pr.get("number")
    if repo is not None and number is not None:
        return by_pr_num_repo.get((repo, number))
    return None


def _target_session_signals(
    session_ids: set[str],
    events_by_session: dict,
    node_rows: list[dict],
    read_transcript,
) -> dict:
    """Aggregate the per-PR signals vector across a node's sessions.

    Structured tier (loop_check / termination events, present) drives the three
    verdict dimensions; ledger + transcript tiers ride as evidence only. An
    unmeasured signal is ``None``, never ``0`` (Locked Decision 6): a node with
    no joined loop_check has ``loop_fires=None``, distinct from a measured zero.

    `promise` is NOT a standalone event kind - it is a ``loop_check`` whose
    ``intent`` is ``"promise"`` (verified against the emitters); likewise the
    plan's ``gate_escape`` / ``phase_transition`` kinds do not exist, so they are
    honestly absent from the vector rather than faked as 0.
    """
    promises = 0
    loop_fires = 0
    ci_by_ts: list[tuple] = []
    terminal_reasons: list[str] = []
    for sid in session_ids:
        for e in events_by_session.get(sid, []):
            kind = e.get("kind") or e.get("type")
            data = e.get("data") if isinstance(e.get("data"), dict) else {}
            if kind == "loop_check":
                loop_fires += 1
                if data.get("intent") == "promise":
                    promises += 1
                dt = _parse_ts(e.get("ts")) or _parse_ts(data.get("ts"))
                ci_by_ts.append((dt or datetime.max, data.get("ci")))
            elif kind == "termination":
                reason = data.get("reason")
                if isinstance(reason, str) and reason:
                    terminal_reasons.append(reason)

    # Ledger rows carry the terminal reason too (the non-52%-loss sessions); union
    # both so a PR whose ledger row is missing still recovers a reason from events.
    for r in node_rows:
        tr = r.get("termination_reason")
        if isinstance(tr, str) and tr:
            terminal_reasons.append(tr)

    ci_by_ts.sort(key=lambda pair: pair[0])
    ci_ordered = [ci for _, ci in ci_by_ts]
    if loop_fires:
        ci_reds, _unparsed = _ci_reds_from_fires(ci_ordered)
    else:
        ci_reds = None  # None (not 0): no joined loop_check != a measured zero

    # Ledger tier (free - already loaded): tokens + duration, nullable so an
    # unrecorded field stays distinct from a measured zero.
    tokens = _sum_opt(r.get("tokens_total") for r in node_rows)
    duration = _sum_opt(r.get("duration_minutes") for r in node_rows)

    # Transcript tier (medium; GC'd -> None). tool-error taxonomy and permission
    # denials are transcript-only, low-tier, and evidence-only per the tiering
    # rule; deferred to a follow-up (they are absent here, never faked as 0).
    # ponytail: turns/toolcalls reuse _transcript_counts; deeper transcript
    # content parsing (error/permission taxonomy) is a separate concern, filed.
    turns = toolcalls = None
    if read_transcript is not None and session_ids:
        turns, toolcalls = _transcript_counts(session_ids, read_transcript)

    return {
        "promises": promises if loop_fires else None,
        "loop_fires": loop_fires or None,
        "ci_reds": ci_reds,
        "terminal_reasons": sorted(set(terminal_reasons)),
        "tokens_total": tokens,
        "duration_minutes": duration,
        "turns": turns,
        "toolcalls": toolcalls,
    }


def _sum_opt(values) -> Optional[float]:
    """Sum a run of possibly-None/junk numerics; all-missing -> None (not 0.0),
    so an unmeasured aggregate stays distinct from a measured zero."""
    total = 0.0
    seen = False
    for v in values:
        if v is None or v == "":
            continue
        try:
            total += float(v)
            seen = True
        except (TypeError, ValueError):
            continue
    return total if seen else None


def build_target_corpus(
    prs: list[dict],
    graph_nodes: list[dict],
    rows: list[dict],
    events_by_session: dict,
    *,
    since_days: int,
    now: datetime,
    postmortems: Optional[list[dict]] = None,
    read_transcript=None,
) -> dict:
    """PR-anchored corpus for the target skill. Pure; the caller pre-fetches
    ``prs`` (``gh pr list`` merged+closed) and ``events_by_session`` (loop_check
    + termination grouped by ``data.session_id``).

    Returns ``{"items", "unattributed", "no_pr", "coverage"}``:
    - ``items``: PRs resolved to a node (scorable). Each carries a signals vector
      and an ``outcome`` from :func:`_node_outcome` (never a reimplemented rule).
    - ``unattributed``: PRs with no resolvable node - counted, all-``None`` vector.
    - ``no_pr``: window sessions that produced no merged/closed PR (survivorship
      correction), bucketed by stop-cause.
    - ``coverage``: the denominator numbers, all with their denominator.

    Invariant (AC1-HP): ``items`` and ``unattributed`` strictly partition ``prs``
    (``len(items) + len(unattributed) == len(prs)``) - asserted here, so a PR
    resolving to neither is a partition bug, not a silent drop.
    """
    postmortems = postmortems or []
    cutoff = now - timedelta(days=since_days)

    by_id = {n.get("id"): n for n in graph_nodes if n.get("id")}
    fixes: dict[str, list[dict]] = {}
    for n in graph_nodes:
        origin = n.get("caused_by")
        if origin:
            fixes.setdefault(origin, []).append(n)
    # Same w4 gate as scoreboard: without any causal telemetry, _node_outcome's
    # "no fix found" branch is indistinguishable from "revert data doesn't exist
    # yet", so a shipped node is unjudgeable rather than a fake merged_clean.
    w4_available = any(("reverted" in n) or n.get("caused_by") for n in graph_nodes)

    by_pr_url: dict = {}
    by_pr_num_repo: dict = {}
    for n in graph_nodes:
        url = n.get("pr_url")
        if url:
            by_pr_url[url] = n
        repo = _repo_from_url(url)
        num = n.get("pr_number")
        if num is not None and repo is not None:
            by_pr_num_repo[(repo, num)] = n
        # additional_prs: a node's secondary PRs (34 nodes carry these), numbered
        # in the node's own repo. Without indexing them a non-primary PR resolves
        # to no node and is wrongly counted unattributed. setdefault so a node's
        # PRIMARY (pr_number) claim always wins a collision over a secondary.
        if repo is not None:
            for ap in n.get("additional_prs") or []:
                apn = ap.get("number") if isinstance(ap, dict) else None
                if apn is not None:
                    by_pr_num_repo.setdefault((repo, apn), n)

    pm_by_node: dict[str, str] = {}
    for pm in postmortems:
        kind = pm.get("blocked_reason_kind")
        if kind and pm.get("graph_node_id"):
            pm_by_node[pm["graph_node_id"]] = kind

    # Index ledger rows by node id (for node -> sessions + terminal reason).
    rows_by_node: dict[str, list[dict]] = {}
    for r in rows:
        nid = r.get("graph_node_id")
        if nid:
            rows_by_node.setdefault(nid, []).append(r)

    items: list[dict] = []
    unattributed: list[dict] = []
    scored_session_ids: set[str] = set()

    for pr in prs:
        node = _resolve_pr_node(pr, by_id, by_pr_url, by_pr_num_repo)
        ship_ts = _parse_ts(pr.get("mergedAt")) or _parse_ts(pr.get("closedAt"))
        merged = bool(pr.get("mergedAt"))
        base = {
            "pr_number": pr.get("number"),
            "pr_url": pr.get("url"),
            "repo": _repo_from_url(pr.get("url")),
            "merged": merged,
        }
        if node is None:
            unattributed.append({**base, "graph_node_id": None})
            continue

        nid = node.get("id")
        node_rows = rows_by_node.get(nid, [])
        session_ids: set[str] = set()
        for s in node.get("sessions") or []:  # x-b6e4 phase stamps: dicts, or legacy bare strings
            if isinstance(s, dict) and isinstance(s.get("session_id"), str) and s["session_id"]:
                session_ids.add(s["session_id"])
            elif isinstance(s, str) and s:  # older graph rows stored sessions as [str]
                session_ids.add(s)
        for r in node_rows:
            session_ids |= _row_session_ids(r)
        scored_session_ids |= session_ids

        signals = _target_session_signals(session_ids, events_by_session, node_rows, read_transcript)

        # shipped_outcome label: ONLY from _node_outcome over w4 telemetry, and
        # only for a MERGED PR. A closed-unmerged PR (or missing telemetry) is
        # unjudgeable - the PR's own merge state is never promoted to a verdict
        # (AC2-ERR). The merge is the anchor; the label is telemetry.
        judgeable = bool(merged and nid and w4_available and nid in by_id)
        outcome = _node_outcome(nid, ship_ts, by_id, fixes) if judgeable else None

        items.append(
            {
                **base,
                "graph_node_id": nid,
                "session_ids": sorted(session_ids),
                "signals": signals,
                "terminal_reasons": signals["terminal_reasons"],
                "judgeable": judgeable,
                "outcome": outcome,
                "attribution_class": classify_postmortem(pm_by_node.get(nid)),
            }
        )

    assert len(items) + len(unattributed) == len(prs), (
        f"PR partition violation: {len(items)} + {len(unattributed)} != {len(prs)}"
    )

    # no-PR class (survivorship correction, US3): window ledger rows that hit an
    # abandoned terminal (_NO_PR_TERMINALS) and whose sessions do not belong to
    # any scored PR - the churny / abandoned / crash-terminated runs the
    # 84%-green stat silently dropped. A shipped row that does not join a scored
    # PR is instead a window/truncation coverage gap (not counted here). The
    # ledger is the honest source for *attempts* even though it is lossy for
    # *deliveries* - that inversion is the whole point. ponytail: a tighter
    # fno:target-only filter needs the GC-lossy transcript scan, which would
    # UNDER-count and reintroduce the survivorship bias this class exists to
    # correct (Open Question 2); abandoned-terminal rows is the honest v1 class.
    no_pr: list[dict] = []
    for r in rows:
        if r.get("termination_reason") not in _NO_PR_TERMINALS:
            continue
        dt = _parse_ts(r.get("completed"))
        if dt is None or not (cutoff <= dt <= now):
            continue
        sids = _row_session_ids(r)
        if sids & scored_session_ids:
            continue
        no_pr.append({"session_ids": sorted(sids), "termination_reason": r["termination_reason"], "graph_node_id": r.get("graph_node_id")})

    stop_cause = dict(Counter(e["termination_reason"] for e in no_pr))

    coverage = {
        "prs_total": len(prs),
        "attributed": len(items),
        "unattributed_pr": len(unattributed),
        "no_pr_attempts": len(no_pr),
        "no_pr_stop_cause": stop_cause,
    }
    return {"items": items, "unattributed": unattributed, "no_pr": no_pr, "coverage": coverage}


def score_target_item(item: dict) -> dict[str, Optional[str]]:
    """Score one PR-anchored corpus item on the three verdict dimensions.

    ``None`` for a dimension means "not scorable" (a coverage gap), never a
    fabricated verdict. Only these three emit verdicts; the rest of the signals
    vector rides as evidence/distribution (Locked Decisions 6, 8). No process
    dimension is ever back-filled from the outcome (AC2-EDGE): each judges only
    its own tier-1 signal.
    """
    result: dict[str, Optional[str]] = {}
    signals = item.get("signals") or {}

    # shipped_outcome (REUSED classifier): merged_clean -> pass; bounced/reverted
    # -> fail if plan-attributable else degraded (attribution-dependent middle
    # state); unjudgeable -> None.
    if not item.get("judgeable"):
        result["shipped_outcome"] = None
    else:
        outcome = item.get("outcome")
        if outcome == "merged_clean":
            result["shipped_outcome"] = "pass"
        elif outcome in ("bounced", "reverted"):
            result["shipped_outcome"] = (
                "fail" if item.get("attribution_class") == "plan-attributable" else "degraded"
            )
        else:
            result["shipped_outcome"] = None

    # first_try_green: 0 ci-red episodes -> pass, >=1 -> fail, no loop_check
    # joined (or unparsed ci drift -> ci_reds None) -> None. Hard structured signal.
    ci_reds = signals.get("ci_reds")
    if signals.get("loop_fires") and ci_reds is not None:
        result["first_try_green"] = "pass" if ci_reds == 0 else "fail"
    else:
        result["first_try_green"] = None

    # converged: a ship reason -> pass; a wedge reason (and no ship) -> fail; no
    # terminal reason -> None. NEVER inferred from the PR being merged.
    reasons = item.get("terminal_reasons") or []
    if any(_is_shipped_reason(r) for r in reasons):
        result["converged"] = "pass"
    elif any(r in _WEDGE_REASONS for r in reasons):
        result["converged"] = "fail"
    else:
        result["converged"] = None

    return result


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

    # -- target: PR-anchored corpus (x-6ff0) --------------------------------- #
    t_now = datetime(2026, 7, 18, 12, 0, 0)
    t_nodes = [
        {"id": "x-a", "pr_number": 10, "pr_url": "https://github.com/o/r/pull/10", "reverted": False,
         "sessions": [{"session_id": "sa", "phase": "ship"}]},
        {"id": "x-b", "pr_number": 11, "pr_url": "https://github.com/o/r/pull/11", "reverted": False,
         "sessions": [{"session_id": "sb", "phase": "ship"}]},
        # a fix-node bouncing x-b within the follow-up window
        {"id": "x-fix", "caused_by": "x-b", "created_at": "2026-07-15T10:00:00"},
    ]
    t_prs = [
        {"number": 10, "headRefName": "feature/x-a", "mergedAt": "2026-07-10T10:00:00Z", "closedAt": None,
         "url": "https://github.com/o/r/pull/10", "state": "MERGED"},
        {"number": 11, "headRefName": "feature/x-b", "mergedAt": "2026-07-11T10:00:00Z", "closedAt": None,
         "url": "https://github.com/o/r/pull/11", "state": "MERGED"},
        {"number": 12, "headRefName": "hotfix/manual", "mergedAt": "2026-07-12T10:00:00Z", "closedAt": None,
         "url": "https://github.com/o/r/pull/12", "state": "MERGED"},  # unattributable
    ]
    t_events = {
        "sa": [{"type": "loop_check", "ts": "2026-07-10T09:00:00Z", "data": {"session_id": "sa", "intent": "promise", "ci": "SUCCESS"}},
               {"type": "termination", "ts": "2026-07-10T09:30:00Z", "data": {"session_id": "sa", "reason": "DonePRGreen"}}],
        "sb": [{"type": "loop_check", "ts": "2026-07-11T08:00:00Z", "data": {"session_id": "sb", "ci": "FAILURE:unit"}},
               {"type": "loop_check", "ts": "2026-07-11T08:05:00Z", "data": {"session_id": "sb", "intent": "promise", "ci": "SUCCESS"}}],
    }
    t_rows = [
        # a churny build attempt in-window with NO PR -> no_pr class
        {"completed": "2026-07-14T10:00:00", "termination_reason": "NoProgress", "phases_completed": ["do"], "sessions": ["snope"]},
        # a plan-only thread -> NOT a no_pr attempt
        {"completed": "2026-07-14T11:00:00", "termination_reason": "DoneAdvisory", "phases_completed": ["think"], "sessions": ["splan"]},
    ]
    corpus = build_target_corpus(t_prs, t_nodes, t_rows, t_events, since_days=28, now=t_now)
    # AC1-HP: strict partition
    assert len(corpus["items"]) == 2 and len(corpus["unattributed"]) == 1, corpus["coverage"]
    assert corpus["coverage"]["prs_total"] == 3
    assert corpus["coverage"]["attributed"] + corpus["coverage"]["unattributed_pr"] == 3
    # AC3-HP: no-PR class present, plan-only excluded
    assert corpus["coverage"]["no_pr_attempts"] == 1, corpus["no_pr"]
    assert corpus["coverage"]["no_pr_stop_cause"] == {"NoProgress": 1}
    item_a = next(i for i in corpus["items"] if i["graph_node_id"] == "x-a")
    item_b = next(i for i in corpus["items"] if i["graph_node_id"] == "x-b")
    # AC2-HP: outcome from the reused classifier - x-a clean, x-b bounced by the fix-node
    assert item_a["outcome"] == "merged_clean", item_a
    assert item_b["outcome"] == "bounced", item_b
    sa = score_target_item(item_a)
    sb = score_target_item(item_b)
    assert sa["shipped_outcome"] == "pass" and sa["first_try_green"] == "pass" and sa["converged"] == "pass", sa
    # x-b: bounced + attribution unknown -> degraded; one ci-red episode -> fail;
    # no ship terminal reason joined -> converged None (NOT back-filled from merge)
    assert sb["shipped_outcome"] == "degraded", sb
    assert sb["first_try_green"] == "fail", sb
    assert sb["converged"] is None, sb  # AC2-EDGE: no terminal reason -> None, never inferred from merge

    # AC2-EDGE: GC'd transcript / no events joined -> structured signals None, no verdict
    t_gc_nodes = [{"id": "x-c", "pr_number": 20, "pr_url": "https://github.com/o/r/pull/20", "reverted": False, "caused_by": None}]
    t_gc_prs = [{"number": 20, "headRefName": "feature/x-c", "mergedAt": "2026-07-13T10:00:00Z", "closedAt": None,
                 "url": "https://github.com/o/r/pull/20", "state": "MERGED"}]
    gc = build_target_corpus(t_gc_prs, t_gc_nodes, [], {}, since_days=28, now=t_now)
    gc_item = gc["items"][0]
    assert gc_item["signals"]["loop_fires"] is None and gc_item["signals"]["ci_reds"] is None, gc_item["signals"]
    gc_scores = score_target_item(gc_item)
    assert gc_scores["first_try_green"] is None and gc_scores["converged"] is None, gc_scores
    # w4 IS available (x-c has caused_by key) and merged+clean -> shipped_outcome pass
    assert gc_scores["shipped_outcome"] == "pass", gc_scores
    # tool-error / permission signals are ABSENT (not 0) from the vector
    assert "tool_errors" not in gc_item["signals"] and "permission_denials" not in gc_item["signals"]

    # AC2-ERR: no w4 telemetry graph-wide -> shipped_outcome None, merge never promoted
    t_now4_nodes = [{"id": "x-d", "pr_number": 30, "pr_url": "https://github.com/o/r/pull/30"}]  # no reverted key, no caused_by
    t_now4_prs = [{"number": 30, "headRefName": "feature/x-d", "mergedAt": "2026-07-13T10:00:00Z", "closedAt": None,
                   "url": "https://github.com/o/r/pull/30", "state": "MERGED"}]
    now4 = build_target_corpus(t_now4_prs, t_now4_nodes, [], {}, since_days=28, now=t_now)
    assert score_target_item(now4["items"][0])["shipped_outcome"] is None, "no-w4 must stay None, never merged_clean"

    print("ok")
