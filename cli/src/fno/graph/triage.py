"""fno backlog triage - backlog reasoning adviser loop.

Ported from ``scripts/triage.py`` (pre-v2). The deterministic parts live
here; LLM reasoning happens in the caller (the /triage skill or /target
wizard).

Subcommands:
    context [--deep] [--all] [--project NAME] [--roadmap-id ID]
    propose [--dry-run] [--deep] [--all] [--project NAME] [--roadmap-id ID]
    validate <proposal.json>
    apply <proposal.json> [--pick ID1,ID2,...]
    projects [--roadmap-id ID]

Exit codes:
    0  success
    2  runtime error (malformed proposal, unreadable graph)
    3  validation dropped one or more edges (apply still lands valid ones)
"""
from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Optional

import typer

from fno import paths as _paths


cli = typer.Typer(
    name="triage",
    help="Backlog reasoning adviser loop",
    no_args_is_help=True,
)


# ---------------------------------------------------------------------------
# Helpers (direct port of scripts/triage.py logic onto the v2 package)
# ---------------------------------------------------------------------------


def _graph_path() -> Path:
    """Return the active graph.json path (monkeypatch-friendly)."""
    from fno.graph._constants import GRAPH_JSON

    return GRAPH_JSON


# Window for the done-not-merged invariant. A bad close is worth catching in the
# days after it lands; older ones are settled, and the bound is what keeps this
# affordable on a cadenced check (984 closed-with-PR nodes in a mature graph).
DONE_NOT_MERGED_WINDOW_DAYS = 7


def _pr_states_by_repo(pr_refs: list[tuple], *, limit: int = 200) -> tuple[dict, list[str]]:
    """Map (repo, pr_number) -> gh state for a batch of refs.

    ONE `gh pr list` per repo rather than one `gh pr view` per node: a per-node
    query would be hundreds of round-trips every time the check runs, which is
    how a cadenced check ends up disabled.

    Returns (states, outage_repos). A repo that could not be read contributes no
    states and its slug lands in outage_repos, so its nodes read as unknown
    rather than as violations - one network blip must never manufacture a dozen
    false alarms.
    """
    from fno.graph._reconcile import ReconcileError, _gh_executable
    import subprocess

    states: dict = {}
    outages: list[str] = []
    if _gh_executable() is None:
        return states, sorted({repo for repo, _ in pr_refs if repo})

    for repo in sorted({repo for repo, _ in pr_refs if repo}):
        cmd = [
            "gh", "pr", "list", "--state", "all", "--limit", str(limit),
            "--repo", repo, "--json", "number,state",
        ]
        try:
            result = subprocess.run(
                cmd, capture_output=True, text=True, check=False, timeout=30
            )
            if result.returncode != 0:
                raise ReconcileError((result.stderr or "").strip() or "non-zero exit")
            rows = json.loads(result.stdout or "[]")
        except (subprocess.TimeoutExpired, OSError, ReconcileError, json.JSONDecodeError):
            outages.append(repo)
            continue
        for row in rows if isinstance(rows, list) else []:
            number = row.get("number")
            if isinstance(number, int):
                states[(repo, number)] = row.get("state")
    return states, outages


def _forced_close_node_ids(roots=None) -> set:
    """Node ids carrying a backlog_done_forced receipt.

    A deliberate force-close over an unmerged PR is the documented bypass, not a
    violation - it is the one close that leaves a reason on the record.

    ``roots`` names extra project roots to read receipts from: under ``--all``
    or a foreign ``--project`` a node's receipt lives in ITS repo's events log,
    not the invocation repo's, so reading only the invocation repo would report
    a legitimately force-closed foreign node as a violation. None reads the
    invocation repo alone (the same-project default).
    """
    ids: set = set()
    paths: list[Path] = []
    seen: set = set()

    def _add(p: Path) -> None:
        if str(p) not in seen:
            seen.add(str(p))
            paths.append(p)

    for r in roots or []:
        _add(Path(r) / ".fno" / "events.jsonl")
    _add(_events_path())  # always include the invocation repo

    for path in paths:
        try:
            if not path.exists():
                continue
            with path.open() as fh:
                for line in fh:
                    if "backlog_done_forced" not in line:
                        continue
                    try:
                        ev = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    # `data` is where the canonical envelope puts it; the other
                    # two are tolerated for older lines.
                    nid = (
                        (ev.get("data") or {}).get("node_id")
                        or (ev.get("payload") or {}).get("node_id")
                        or ev.get("node_id")
                    )
                    if nid:
                        ids.add(nid)
        except OSError:
            continue
    return ids


def done_not_merged_report(entries: list[dict], *, window_days: int = DONE_NOT_MERGED_WINDOW_DAYS) -> dict:
    """Nodes closed over a PR that is not merged.

    The invariant this asserts: for every node with `pr_number` and
    `completed_at` set, the referenced PR must be MERGED, or the node must carry
    a forced-close receipt.

    It is stated about the graph rather than enforced inside a close function on
    purpose - a guard in `cmd_done` cannot see a writer that never calls
    `cmd_done`, which is exactly how nodes came to be stamped done over open PRs
    while the guarded door was working correctly.

    Scoped to `pr_number is not None`, so advisory and doc nodes (legitimately
    PR-less) are exempt rather than permanently red.
    """
    from datetime import datetime, timedelta, timezone

    from fno.graph._reconcile import node_pr_refs, repo_slug_from_url

    now = datetime.now(timezone.utc)
    cutoff = now - timedelta(days=window_days)

    # (node, [(repo, pr_number, pr_url), ...]) - EVERY ref, not just the primary.
    # cmd_done closes on any MERGED ref (a node whose primary is OPEN but whose
    # additional_prs carries a merged PR is a legitimate close), so checking the
    # primary alone reports a valid close as a violation.
    candidates: list[tuple] = []
    for e in entries:
        completed_at, pr_number = e.get("completed_at"), e.get("pr_number")
        if not completed_at or not isinstance(pr_number, int):
            continue
        try:
            closed_dt = datetime.fromisoformat(str(completed_at).replace("Z", "+00:00"))
        except ValueError:
            continue
        if closed_dt < cutoff:
            continue
        refs = [(repo_slug_from_url(url or ""), num, url) for num, url in node_pr_refs(e)]
        candidates.append((e, refs))

    if not candidates:
        return {"violations": [], "unknown": [], "checked": 0, "window_days": window_days}

    all_pairs = [(repo, num) for _, refs in candidates for repo, num, _ in refs]
    states, outage_repos = _pr_states_by_repo(all_pairs)
    roots = {r for e, _ in candidates if (r := (e.get("_resolved_cwd") or e.get("cwd")))}
    forced = _forced_close_node_ids(roots)

    violations: list[dict] = []
    unknown: list[dict] = []
    for node, refs in candidates:
        record = {
            "id": node.get("id"),
            "title": node.get("title", ""),
            "pr_number": refs[0][1],  # primary is the node's identity
            "completed_at": node.get("completed_at"),
        }
        if node.get("id") in forced:
            continue

        ref_states = [states.get((repo, num)) for repo, num, _ in refs]
        if "MERGED" in ref_states:
            continue  # any merged ref is a valid close

        # No merged ref. If ANY ref is unreadable, a merged ref could be hiding
        # behind it, so this stays unknown - the same never-a-false-breach rule
        # the gh-outage case follows.
        if None in ref_states:
            reason = "unknown"
            for (repo, _num, _url), st in zip(refs, ref_states):
                if st is None:
                    if not repo:
                        reason = "no pr_url to resolve the repo"
                    elif repo in outage_repos:
                        reason = "gh outage"
                    else:
                        reason = "not in gh window"
                    break
            unknown.append({**record, "reason": reason})
            continue

        # Every ref read, none merged: a genuine close over unmerged evidence.
        violations.append({**record, "pr_state": ref_states[0]})

    return {
        "violations": violations,
        "unknown": unknown,
        "checked": len(candidates),
        "window_days": window_days,
    }


def _events_path() -> Path:
    """Repo-root ``.fno/events.jsonl`` - the same file ``fno event emit`` writes
    to (events/cli.py) and the routing/triage fold reads. Anchoring to the repo
    root (not cwd) keeps producer and consumer coincident regardless of which
    subdirectory a verb runs from."""
    try:
        from fno.paths import resolve_repo_root

        return resolve_repo_root() / ".fno" / "events.jsonl"
    except Exception:
        return Path(".fno/events.jsonl")


def _read_canonical_events(path: Optional[Path] = None) -> list[dict]:
    """Tolerant read of the canonical ``{ts,type,source,data}`` events log.
    Best-effort: a malformed line is skipped, never raised - the health fold is
    advisory and must not break because one event row is corrupt."""
    p = path or _events_path()
    if not p.exists():
        return []
    out: list[dict] = []
    try:
        lines = p.read_text(encoding="utf-8").splitlines()
    except OSError:
        return []
    for raw in lines:
        raw = raw.strip()
        if not raw:
            continue
        try:
            out.append(json.loads(raw))
        except (json.JSONDecodeError, ValueError):
            continue
    return out


def fold_routing_health(events: list[dict]) -> Optional[dict]:
    """Fold ``executor_resolved`` events into routing-tier metrics (x-64cb US3).

    Returns None when no such events exist so the health render can gate the
    section (AC6-EDGE: no fabricated zeros). The override-after-inference count
    is the v1 mis-route PROXY: a task first resolved by surface-inference that a
    later explicit lock (task-block / plan-frontmatter) resolved to a *different*
    executor. Only tasks carrying an id can be correlated; the denominator is
    those tasks, surfaced alongside the numerator so the rate is never bare."""
    er = [e for e in events if e.get("type") == "executor_resolved"]
    if not er:
        return None
    tiers: dict[str, int] = {}
    warn = 0
    # Key by (plan_path, task): task ids like "1.1" are plan-relative, not
    # global, so correlating override-after-inference on the bare task id would
    # cross-contaminate two plans that both have a "1.1" (peer review, PR #285).
    inferred: dict[tuple[str, str], str] = {}
    overridden: set[tuple[str, str]] = set()
    for e in er:
        d = e.get("data", {}) or {}
        tier = d.get("tier", "?")
        tiers[tier] = tiers.get(tier, 0) + 1
        if d.get("warn_fallback"):
            warn += 1
        task = d.get("task") or ""
        if not task:
            continue
        key = (d.get("plan_path") or "", task)
        resolved = d.get("resolved")
        if tier == "surface-inference":
            inferred.setdefault(key, resolved)
        elif tier in ("task-block", "plan-frontmatter"):
            if key in inferred and resolved != inferred[key]:
                overridden.add(key)
    return {
        "total": len(er),
        "tier_distribution": tiers,
        "inference": tiers.get("surface-inference", 0),
        "warn_fallback_count": warn,
        "inferred_tasks": len(inferred),
        "overridden_after_inference": len(overridden),
    }


def fold_triage_health(events: list[dict]) -> Optional[dict]:
    """Fold ``triage_applied`` events into apply-count + validation-drop metrics
    (x-64cb US3). Returns None when absent (event-gated render). The drop rate
    ships both numerator and denominator so it is never a bare percentage."""
    ta = [e for e in events if e.get("type") == "triage_applied"]
    if not ta:
        return None
    cats = {"priority_changes": 0, "dependencies": 0, "duplicates_flagged": 0, "deferred": 0}
    proposed = 0
    dropped = 0
    for e in ta:
        d = e.get("data", {}) or {}
        a = d.get("applied", {}) or {}
        for k in cats:
            try:
                cats[k] += int(a.get(k, 0) or 0)
            except (TypeError, ValueError):
                pass
        try:
            proposed += int(d.get("proposed", 0) or 0)
            dropped += int(d.get("dropped", 0) or 0)
        except (TypeError, ValueError):
            pass
    return {
        "applies": len(ta),
        "applied_by_category": cats,
        "proposed": proposed,
        "dropped": dropped,
    }


# ---------------------------------------------------------------------------
# Consistency measurement (x-64cb US4): run the propose step K times over one
# frozen context and measure per-category agreement. Read-only toward the live
# graph - propose runs never apply.
# ---------------------------------------------------------------------------

# The reasoning instruction handed to each headless run. MUST mirror the
# /triage skill's reasoning prompt (skills/triage/SKILL.md, "LLM reasoning"
# step) so the consistency measurement reflects what production /triage does;
# when one changes, change both (x-64cb US5 hardens the pair together).
_CONSISTENCY_PROMPT = (
    "You are a backlog triage classifier. First REASON, then LABEL - never emit "
    "the JSON first. In a short reasoning pass, name each spec's PRIMARY concern "
    "(when a spec raises several concerns, classify on the primary, not the "
    "loudest surface signal). Then output an optimal ordering as JSON with four "
    "keys: `dependencies` (edges {from,to,reason} where `to` is blocked_by "
    "`from`), `priority_changes` ({id,to,reason} where `to` is one of "
    "p0/p1/p2/p3), `defer` ({id,reason}), and `duplicates` ({ids:[...],reason}). "
    "Every entry MUST include a one-line `reason`. Do not propose self-edges or "
    "cycles. Only reason over the `candidates` array; never propose changes for "
    "`ideas`."
)

_CONSISTENCY_SCHEMA = {
    "type": "object",
    "properties": {
        "dependencies": {"type": "array", "items": {"type": "object"}},
        "priority_changes": {"type": "array", "items": {"type": "object"}},
        "defer": {"type": "array", "items": {"type": "object"}},
        "duplicates": {"type": "array", "items": {"type": "object"}},
    },
    "required": ["priority_changes"],
}


def _run_consistency_propose(context: dict, model: Optional[str]) -> dict:
    """Run ONE headless propose over the frozen context; return the proposal
    dict. Raises on any failure so the caller counts it as an errored run.

    Dispatch is a synchronous headless one-shot on the subscription-OAuth lane -
    the same primitive fno.inbox.triage uses (plain ``claude -p``, which honors
    OAuth; NOT ``--bare``, which needs an API key). Tests set
    ``FNO_TRIAGE_CONSISTENCY_STUB`` to a script that prints proposal JSON so the
    real model is never called under pytest/CI.
    # ponytail: claude-only for v1; provider rotation is a follow-up node.
    """
    import os
    import subprocess

    stub = os.environ.get("FNO_TRIAGE_CONSISTENCY_STUB")
    in_pytest = os.environ.get("PYTEST_CURRENT_TEST") is not None
    in_ci = os.environ.get("CI", "").lower() in ("true", "1", "yes")
    if not stub and (in_pytest or in_ci):
        raise RuntimeError(
            "FNO_TRIAGE_CONSISTENCY_STUB not configured; refusing real claude -p in tests"
        )

    prompt = f"{_CONSISTENCY_PROMPT}\n\nCONTEXT:\n{json.dumps(context)}"
    if stub:
        cmd = [stub]
    else:
        cmd = [
            "claude", "-p",
            "--output-format", "json",
            "--json-schema", json.dumps(_CONSISTENCY_SCHEMA),
            "--append-system-prompt", "You are a triage agent. Respond with JSON only.",
        ]
        if model:
            cmd += ["--model", model]
    result = subprocess.run(
        cmd, input=prompt, capture_output=True, text=True, timeout=300, check=True
    )
    data = json.loads(result.stdout)
    # `claude -p --output-format json --json-schema` returns an envelope
    # {is_error, structured_output, result, ...}; the schema object lives in
    # `structured_output` (or `result` as its JSON text). A test stub prints the
    # proposal object directly. Identify a direct proposal by its defining
    # priority_changes key first, so a stub that happens to carry a `result`
    # field is never misrouted through envelope-unwrapping; only unwrap when the
    # key is absent. is_error first there, since a failure carries no schema.
    proposal = data
    if isinstance(data, dict) and "priority_changes" not in data:
        if data.get("is_error"):
            raise RuntimeError(f"claude -p error: {data.get('result') or data.get('error')}")
        structured, result_text = data.get("structured_output"), data.get("result")
        if isinstance(structured, dict):
            proposal = structured
        elif isinstance(result_text, str):
            try:
                proposal = json.loads(result_text)
            except json.JSONDecodeError as e:
                raise ValueError(f"claude -p result is not JSON: {e}") from e
    if not isinstance(proposal, dict):  # non-dict data, or result parsed to a non-dict
        raise ValueError("proposal is not a JSON object")
    # A schema-conforming proposal always carries priority_changes (required in
    # _CONSISTENCY_SCHEMA), even if empty; its absence is an underfilled envelope
    # that must count as an errored run, not fold as a silent zero agreement.
    if "priority_changes" not in proposal:
        raise ValueError("proposal missing required priority_changes")
    return proposal


def _priority_map(proposal: dict) -> dict[str, object]:
    """{node id -> proposed `to`} for a proposal's priority_changes."""
    out: dict[str, object] = {}
    for pc in proposal.get("priority_changes", []) or []:
        if isinstance(pc, dict) and pc.get("id"):
            out[str(pc["id"])] = pc.get("to")
    return out


def _presence_map(proposal: dict, category: str, key_fn) -> dict[str, bool]:
    """{key -> True} for a category whose agreement is presence-based."""
    out: dict[str, bool] = {}
    for e in proposal.get(category, []) or []:
        if not isinstance(e, dict):
            continue
        k = key_fn(e)
        if k:
            out[k] = True
    return out


def _category_agreement(per_run_maps: list[dict]) -> dict:
    """A key agrees when every completed run assigns it the SAME value (a run
    that omits the key contributes None, so 'some propose, some don't' is a
    disagreement). Returns {agree, total, disagreeing:[keys]}."""
    universe: set = set()
    for m in per_run_maps:
        universe |= set(m.keys())
    agree = 0
    disagreeing: list = []
    for k in sorted(universe, key=str):
        vals = [m.get(k) for m in per_run_maps]
        if all(v == vals[0] for v in vals):
            agree += 1
        else:
            disagreeing.append(k)
    return {"agree": agree, "total": len(universe), "disagreeing": disagreeing}


def fold_consistency(proposals: list[dict]) -> dict:
    """Per-category agreement over the COMPLETED-run proposals (x-64cb US4).
    Priority is keyed by node id + `to` value; the rest are presence-based."""
    return {
        "priority": _category_agreement([_priority_map(p) for p in proposals]),
        "dependencies": _category_agreement(
            [_presence_map(p, "dependencies", lambda e: f"{e.get('from')}->{e.get('to')}") for p in proposals]
        ),
        "defer": _category_agreement(
            [_presence_map(p, "defer", lambda e: str(e.get("id")) if e.get("id") else "") for p in proposals]
        ),
        "duplicates": _category_agreement(
            [_presence_map(p, "duplicates", lambda e: ",".join(sorted(map(str, e.get("ids", []))))) for p in proposals]
        ),
    }


def _emit_triage_applied(
    applied: dict, priority_moves: list[dict], proposed: int, dropped: int
) -> None:
    """Best-effort ``triage_applied`` telemetry (x-64cb US2). The graph mutation
    has already committed by the time this runs; an emit failure logs one stderr
    line and never changes apply semantics or the exit code."""
    import sys

    try:
        from fno.events import _build, append_event

        event = _build(
            "triage_applied",
            "backlog",
            {
                "applied": applied,
                "priority_moves": priority_moves,
                "proposed": proposed,
                "dropped": dropped,
            },
        )
        append_event(event, events_path=_events_path())
    except Exception as exc:  # noqa: BLE001 - telemetry is best-effort
        print(
            f"triage: warning: triage_applied emit failed ({type(exc).__name__}: {exc})",
            file=sys.stderr,
        )


def _is_pending(entry: dict) -> bool:
    """Ready or blocked, not done/deferred/claimed/idea/roadmap-row.

    Ideas are intentionally excluded - the LLM should recommend writing a
    spec for them rather than treating them as claimable work-in-progress.
    Surface them via ``_is_idea`` instead so they live in their own array.

    Deferred rows are also excluded so triage proposals never re-suggest a
    paused node. Re-engagement is an explicit user action via
    ``backlog undefer`` or ``backlog ready --include-deferred``.
    """
    if entry.get("type") == "roadmap":
        return False
    completed = entry.get("completed_at") or ""
    if completed:
        return False
    status = entry.get("status", "ready")
    if status == "deferred":
        return False
    return status in ("ready", "blocked")


def _is_idea(entry: dict) -> bool:
    """Idea-stage row: plan-less, not claimed, not blocked, not done."""
    if entry.get("type") == "roadmap":
        return False
    if entry.get("completed_at"):
        return False
    return entry.get("status") == "idea"


def _read_plan_excerpt(plan_path: Optional[str], max_lines: int = 150) -> str:
    if not plan_path:
        return ""
    try:
        p = Path(plan_path)
        if not p.exists():
            return ""
        with p.open() as f:
            lines: list[str] = []
            for i, line in enumerate(f):
                if i >= max_lines:
                    break
                lines.append(line)
            return "".join(lines)
    except OSError:
        return ""


def _candidate_record(entry: dict, deep: bool) -> dict:
    # Defensive parsing: legacy/corrupted graph entries may carry a
    # non-list cost_sessions (e.g. a dict keyed by session id), or
    # individual sessions where cost_usd is a string, None, or bool.
    # The triage adviser must not crash on a heterogeneous backlog and
    # must not silently report wrong-but-numeric values to the LLM, so
    # we filter to well-formed sessions and align session_count with
    # the same denominator total_cost_usd uses.
    raw_sessions = entry.get("cost_sessions")
    cost_sessions = raw_sessions if isinstance(raw_sessions, list) else []
    valid_sessions = [
        s for s in cost_sessions
        if isinstance(s, dict)
        and isinstance(s.get("cost_usd"), (int, float))
        and not isinstance(s.get("cost_usd"), bool)
    ]
    cost_total = sum(s["cost_usd"] for s in valid_sessions)

    record = {
        "id": entry.get("id"),
        "title": entry.get("title"),
        "priority": entry.get("priority") or "p2",
        "blocked_by": list(entry.get("blocked_by", [])),
        "plan_path": entry.get("plan_path"),
        "roadmap_id": entry.get("roadmap_id"),
        "created_at": entry.get("created_at"),
        "source": entry.get("source"),
        "status": entry.get("status"),
        "size": entry.get("size"),
        "domain": entry.get("domain"),
        "details": entry.get("details"),
        "claim_history": {
            "session_count": len(valid_sessions),
            "total_cost_usd": round(cost_total, 2),
            "last_claimed_at": entry.get("claimed_at"),
        },
        "ship_state": {
            "pr_number": entry.get("pr_number"),
            "merge_status": entry.get("merge_status"),
        },
    }
    if deep and record["plan_path"]:
        record["plan_excerpt"] = _read_plan_excerpt(record["plan_path"])
    return record


def _collect_pending(
    roadmap_id: Optional[str],
    deep: bool,
    project: Optional[str],
    all_projects: bool,
    entries: Optional[list[dict]] = None,
) -> list[dict]:
    """Gather pending nodes, scoped to the current project by default."""
    from fno.graph._constants import PRIORITY_ORDER
    from fno.graph._intake import filter_by_project
    from fno.graph.store import read_graph

    if entries is None:
        entries = read_graph(_graph_path())
    if roadmap_id:
        entries = [e for e in entries if e.get("roadmap_id") == roadmap_id]
    entries = filter_by_project(entries, project, all_projects)
    pending = [e for e in entries if _is_pending(e)]
    pending.sort(
        key=lambda e: (
            PRIORITY_ORDER.get(e.get("priority", "p2"), 2),
            e.get("created_at", ""),
        )
    )
    return [_candidate_record(e, deep) for e in pending]


def _collect_ideas(
    roadmap_id: Optional[str],
    deep: bool,
    project: Optional[str],
    all_projects: bool,
    entries: Optional[list[dict]] = None,
) -> list[dict]:
    """Gather idea-stage nodes scoped the same way as pending candidates."""
    from fno.graph._constants import PRIORITY_ORDER
    from fno.graph._intake import filter_by_project
    from fno.graph.store import read_graph

    if entries is None:
        entries = read_graph(_graph_path())
    if roadmap_id:
        entries = [e for e in entries if e.get("roadmap_id") == roadmap_id]
    entries = filter_by_project(entries, project, all_projects)
    ideas = [e for e in entries if _is_idea(e)]
    ideas.sort(
        key=lambda e: (
            PRIORITY_ORDER.get(e.get("priority", "p2"), 2),
            e.get("created_at", ""),
        )
    )
    return [_candidate_record(e, deep) for e in ideas]


def _collect_inbox_items() -> list[dict]:
    """Gather unchecked fu-* items from the backlog capture tier.

    The two-source picker (AC4-HP): inbox items appear alongside ab-* graph
    nodes, each labelled with its id type so the reasoning layer can route a
    selected fu-* item to `fno backlog capture promote`. Best-effort: a missing
    or unreadable inbox file yields an empty list, never an error.
    """
    try:
        from fno.backlog.capture import parse_items
        from fno.paths import inbox_path

        path = inbox_path()
        if not path.exists():
            return []
        items = parse_items(path.read_text(encoding="utf-8"))
    except Exception:
        return []
    return [
        {
            "id": i["id"],
            "id_type": "fu",
            "title": i["title"],
            "priority": i["priority"],
            "source": "inbox",
        }
        for i in items
    ]


def _load_goals() -> list[dict]:
    """Read project goals from the project config (config.toml, else legacy
    settings.yaml)."""
    # Function-local: keep graph-module load free of config_io's pydantic/yaml.
    from fno.config_io import config_read_candidates, read_config_flat

    candidates = config_read_candidates([Path(".fno/settings.yaml"), _paths.config_file()])
    for path in candidates:
        if not path.exists():
            continue
        data = read_config_flat(path)
        # Config nests goals under `project.goals`, but older schemas had a
        # top-level `goals:` block. Accept either so a mid-migration config
        # still yields useful context.
        project = data.get("project")
        goals = None
        if isinstance(project, dict) and isinstance(project.get("goals"), list):
            goals = project["goals"]
        elif isinstance(data.get("goals"), list):
            goals = data["goals"]
        if isinstance(goals, list) and goals:
            # Normalize to only the keys the LLM reasoning prompt uses
            # (id/goal/status) so downstream consumers stay stable.
            return [
                {k: g[k] for k in ("id", "goal", "status") if k in g}
                for g in goals
                if isinstance(g, dict)
            ]
    return []


def _resolve_scope(
    project: Optional[str],
    all_projects: bool,
    entries: Optional[list[dict]] = None,
) -> str:
    from fno.graph._intake import detect_project
    from fno.graph.store import read_graph

    if project:
        return f"project '{project}'"
    if all_projects:
        return "all projects"
    snapshot = entries if entries is not None else read_graph(_graph_path())
    detected = detect_project(snapshot)
    if detected:
        return f"project '{detected}' (auto-detected)"
    return "all projects (no project detected - run an intake to register this repo)"


def _load_proposal(path: Path) -> dict:
    """Load a proposal JSON file with a clean error message on malformed input."""
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        typer.echo(f"Error: proposal file not found: {path}", err=True)
        raise typer.Exit(code=2)
    except json.JSONDecodeError as e:
        typer.echo(f"Error: proposal is not valid JSON ({path}): {e}", err=True)
        raise typer.Exit(code=2)


def _copeland_rank(
    ids: list[str],
    verdicts: list[dict],
    meta: Optional[dict[str, tuple]] = None,
) -> list[dict]:
    """Order ``ids`` best-first from pairwise verdicts via Copeland score.

    Tournament ordering: comparative judgment ("ship X or Y first?") is more
    reliable than one-shot absolute scoring for qualitative ranking. Each
    verdict is ``{"winner": id, "loser": id}``. The Copeland
    score is ``wins - losses``; contradictory or cyclic verdicts are tolerated
    (they net out) rather than rejected, and a verdict naming an id outside
    ``ids`` is ignored. Deterministic tiebreak: higher net, then ``meta``
    (priority rank asc, created_at asc), then id.

    ponytail: Copeland over an explicit verdict list, not Elo/Swiss. No
    iterative rating, no match scheduling - the skill enumerates the pairs and
    the LLM judges them; this just folds the answers into one stable order.
    """
    idset = list(dict.fromkeys(ids))  # de-dupe, preserve first-seen order
    present = set(idset)
    wins = {i: 0 for i in idset}
    losses = {i: 0 for i in idset}
    for v in verdicts:
        if not isinstance(v, dict):
            continue
        win, lose = v.get("winner"), v.get("loser")
        # isinstance guard before the membership test: a non-str (list/dict)
        # value from malformed JSON is unhashable and would raise TypeError on
        # `in present`. Verdicts are external input, so validate at the boundary.
        if (
            isinstance(win, str)
            and isinstance(lose, str)
            and win in present
            and lose in present
            and win != lose
        ):
            wins[win] += 1
            losses[lose] += 1
    meta = meta or {}

    def _key(i: str):
        prio, created = meta.get(i, (99, ""))
        return (-(wins[i] - losses[i]), prio, created, i)

    return [
        {"id": i, "wins": wins[i], "losses": losses[i], "net": wins[i] - losses[i]}
        for i in sorted(idset, key=_key)
    ]


def _build_dependency_map(entries: list[dict]) -> dict[str, set[str]]:
    result: dict[str, set[str]] = {}
    for e in entries:
        eid = e.get("id")
        if isinstance(eid, str):
            result[eid] = set(b for b in e.get("blocked_by", []) if isinstance(b, str))
    return result


def _would_cycle(graph: dict[str, set[str]], frm: str, to: str) -> bool:
    """True if adding edge ``to blocked_by frm`` creates a cycle."""
    stack = [frm]
    seen: set[str] = set()
    while stack:
        node = stack.pop()
        if node == to:
            return True
        if node in seen:
            continue
        seen.add(node)
        for blocker in graph.get(node, ()):
            stack.append(blocker)
    return False


def _validate_proposal(
    proposal: dict,
    entries: list[dict],
) -> tuple[dict, list[str]]:
    """Return (cleaned, errors). Drops cycles and unknown-id entries."""
    from fno.graph._constants import PRIORITY_ORDER

    errors: list[str] = []
    valid_ids = {e.get("id") for e in entries if isinstance(e.get("id"), str)}

    deps_in = proposal.get("dependencies", []) or []
    dep_map = _build_dependency_map(entries)
    clean_deps: list[dict] = []
    for edge in deps_in:
        if not isinstance(edge, dict):
            errors.append(f"dependency entry is not an object: {edge!r}")
            continue
        frm = edge.get("from")
        to = edge.get("to")
        if frm not in valid_ids or to not in valid_ids:
            errors.append(f"dependency references unknown id(s): {frm} -> {to}")
            continue
        if frm == to:
            errors.append(f"self-dependency ignored: {frm}")
            continue
        if _would_cycle(dep_map, frm, to):
            errors.append(
                f"cycle: adding {to} blocked_by {frm} would cycle - dropping edge"
            )
            continue
        dep_map.setdefault(to, set()).add(frm)
        clean_deps.append(edge)

    prio_in = proposal.get("priority_changes", []) or []
    clean_prio: list[dict] = []
    for pc in prio_in:
        if not isinstance(pc, dict):
            errors.append(f"priority_change entry is not an object: {pc!r}")
            continue
        pid = pc.get("id")
        to_p = pc.get("to")
        if pid not in valid_ids:
            errors.append(f"priority_change references unknown id: {pid}")
            continue
        if to_p not in PRIORITY_ORDER:
            errors.append(f"priority_change invalid priority: {to_p!r}")
            continue
        clean_prio.append(pc)

    dups_in = proposal.get("duplicates", []) or []
    clean_dups: list[dict] = []
    for dup in dups_in:
        if not isinstance(dup, dict):
            errors.append(f"duplicate entry is not an object: {dup!r}")
            continue
        ids = dup.get("ids") or []
        if not isinstance(ids, list) or len(ids) < 2:
            errors.append(
                f"duplicate entry requires ids list with >=2 elements: {dup!r}"
            )
            continue
        bad = [i for i in ids if i not in valid_ids]
        if bad:
            errors.append(f"duplicate references unknown id(s): {bad}")
            continue
        clean_dups.append(dup)

    # Defer entries: ``{"id": "ab-X", "reason": "..."}``. Reason is required
    # so a paused node always carries the rationale into the kanban view; an
    # entry with a blank or missing reason is dropped.
    defer_in = proposal.get("defer", []) or []
    clean_defer: list[dict] = []
    for d in defer_in:
        if not isinstance(d, dict):
            errors.append(f"defer entry is not an object: {d!r}")
            continue
        did = d.get("id")
        reason = (d.get("reason") or "").strip()
        if did not in valid_ids:
            errors.append(f"defer references unknown id: {did}")
            continue
        if not reason:
            errors.append(f"defer entry missing reason: {did}")
            continue
        clean_defer.append({"id": did, "reason": reason})

    cleaned = {
        "dependencies": clean_deps,
        "priority_changes": clean_prio,
        "duplicates": clean_dups,
        "defer": clean_defer,
    }
    return cleaned, errors


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------


def _build_context(
    deep: bool,
    all_projects: bool,
    project: Optional[str],
    roadmap_id: Optional[str],
) -> dict:
    """Build the LLM-reasoning context payload. Shared by `triage context` and
    `triage consistency` so both reason over an identical snapshot."""
    from fno.graph.store import read_graph

    entries = read_graph(_graph_path())
    candidates = _collect_pending(roadmap_id, deep, project, all_projects, entries)
    ideas = _collect_ideas(roadmap_id, deep, project, all_projects, entries)
    inbox_items = _collect_inbox_items()
    return {
        "candidates": candidates,
        "ideas": ideas,
        "inbox_items": inbox_items,
        "goals": _load_goals(),
        "mode": "deep" if deep else "shallow",
        "count": len(candidates),
        "idea_count": len(ideas),
        "inbox_count": len(inbox_items),
        "scope": _resolve_scope(project, all_projects, entries),
    }


@cli.command("context")
def cmd_context(
    deep: bool = typer.Option(False, "--deep", help="Include plan excerpts"),
    all_projects: bool = typer.Option(
        False, "--all", "-A", help="Include pending nodes from all projects"
    ),
    project: Optional[str] = typer.Option(
        None, "--project", help="Filter by project (default: auto-detect)"
    ),
    roadmap_id: Optional[str] = typer.Option(
        None, "--roadmap-id", help="Filter by roadmap ID"
    ),
) -> None:
    """Emit JSON context for an LLM reasoning subagent."""
    typer.echo(json.dumps(_build_context(deep, all_projects, project, roadmap_id), indent=2))


@cli.command("propose")
def cmd_propose(
    dry_run: bool = typer.Option(
        False, "--dry-run", "-N", help="Heuristic-only, no LLM call"
    ),
    deep: bool = typer.Option(False, "--deep", help="Include plan excerpts"),
    all_projects: bool = typer.Option(
        False, "--all", "-A", help="Include pending nodes from all projects"
    ),
    project: Optional[str] = typer.Option(
        None, "--project", help="Filter by project (default: auto-detect)"
    ),
    roadmap_id: Optional[str] = typer.Option(
        None, "--roadmap-id", help="Filter by roadmap ID"
    ),
) -> None:
    """Emit a proposal skeleton or a dry-run candidate summary."""
    from fno.graph.store import read_graph

    entries = read_graph(_graph_path())
    scope = _resolve_scope(project, all_projects, entries)
    candidates = _collect_pending(roadmap_id, deep, project, all_projects, entries)
    ideas = _collect_ideas(roadmap_id, deep, project, all_projects, entries)

    proposal = {
        "dependencies": [],
        "priority_changes": [],
        "duplicates": [],
        "defer": [],
        "candidates": candidates,
        "ideas": ideas,
        "scope": scope,
    }

    if not candidates:
        typer.echo(f"no pending nodes to triage (scope: {scope})", err=True)
        typer.echo(json.dumps(proposal, indent=2))
        return

    if dry_run:
        header = [
            f"Proposed triage for {len(candidates)} pending nodes",
            f"Scope: {scope}",
            "(dry-run: no LLM call, showing candidates only)",
            "",
        ]
        for c in candidates:
            header.append(f"  {c['id']} [{c['priority']}] {c['title']}")
        typer.echo("\n".join(header), err=True)

    typer.echo(json.dumps(proposal, indent=2))


@cli.command("consistency")
def cmd_consistency(
    repeat: int = typer.Option(
        3, "--repeat", "-k", help="Number of propose runs over the frozen context"
    ),
    frozen_context: Optional[Path] = typer.Option(
        None, "--frozen-context", help="Pre-captured context JSON; skips capture"
    ),
    yes: bool = typer.Option(False, "--yes", help="Confirm --repeat > 10 (cost guard)"),
    model: Optional[str] = typer.Option(None, "--model", help="Model for the headless runs"),
    deep: bool = typer.Option(False, "--deep"),
    all_projects: bool = typer.Option(False, "--all", "-A"),
    project: Optional[str] = typer.Option(None, "--project"),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Measure triage classification consistency: run the propose step K times
    over ONE frozen context snapshot and report per-category agreement plus the
    node ids that disagreed. Read-only - propose runs never touch the graph."""
    if repeat < 1:
        raise typer.BadParameter("--repeat must be >= 1")

    # One snapshot, read by all K runs (Invariant: never the live graph).
    if frozen_context is not None:
        context = json.loads(frozen_context.read_text(encoding="utf-8"))
    else:
        context = _build_context(deep, all_projects, project, roadmap_id)

    if not context.get("candidates"):
        typer.echo("nothing to propose (no candidates in the frozen context)", err=True)
        return  # exit 0, no LLM calls (Boundaries)

    # Cost guard AFTER the empty-context short-circuit: an empty context makes
    # zero LLM calls, so it should never demand --yes (peer review, PR #285).
    if repeat > 10 and not yes:
        typer.echo(
            f"--repeat {repeat} makes {repeat} real LLM calls; pass --yes to confirm.",
            err=True,
        )
        raise typer.Exit(code=2)

    proposals: list[dict] = []
    errored = 0
    for i in range(repeat):
        try:
            proposals.append(_run_consistency_propose(context, model))
        except Exception as exc:  # noqa: BLE001 - one errored run must not abort the rest (AC7-FR)
            errored += 1
            typer.echo(f"run {i + 1}/{repeat} errored: {type(exc).__name__}: {exc}", err=True)

    agreement = fold_consistency(proposals) if proposals else {}
    report = {
        "repeat": repeat,
        "completed": len(proposals),
        "errored": errored,
        "agreement": agreement,
    }
    if json_output:
        typer.echo(json.dumps(report, indent=2))
        return

    typer.echo(
        f"Triage consistency: {len(proposals)}/{repeat} runs completed ({errored} errored)"
    )
    if repeat == 1:
        typer.echo("  note: K=1 measures nothing (a single run trivially agrees with itself)")
    if not proposals:
        typer.echo("  no completed runs; agreement not computed")
        return
    for cat, ag in agreement.items():
        if ag["total"] == 0:
            continue
        typer.echo(f"  {cat}: {ag['agree']}/{ag['total']} agree")
        if ag["disagreeing"]:
            typer.echo(f"    disagreeing: {', '.join(map(str, ag['disagreeing']))}")


@cli.command("rank")
def cmd_rank(
    verdicts: Optional[Path] = typer.Option(
        None,
        "--verdicts",
        help=(
            'JSON file of pairwise verdicts [{"winner": id, "loser": id}, ...]. '
            "Reads stdin when omitted."
        ),
    ),
) -> None:
    """Fold pairwise comparison verdicts into one total order (Copeland).

    The deterministic seam of tournament triage: the /triage skill judges
    candidate PAIRS with the LLM (comparative judgment beats one-shot
    absolute scoring), then this verb aggregates those verdicts into a single
    consistent order, tolerating the occasional contradictory/cyclic verdict.
    Apply the resulting order with ``fno backlog rank --top/--after``.

    Participants are the verdict ids that exist in the graph (unknown ids are
    dropped, mirroring validate). Output is JSON:
    ``{"order": [{"id", "title", "wins", "losses", "net"}, ...]}`` best-first.
    """
    import sys

    from fno.graph._constants import PRIORITY_ORDER
    from fno.graph.store import read_graph

    try:
        raw = verdicts.read_text() if verdicts else sys.stdin.read()
    except OSError as e:
        typer.echo(f"Error: could not read verdicts: {e}", err=True)
        raise typer.Exit(code=2)
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as e:
        typer.echo(f"Error: verdicts are not valid JSON: {e}", err=True)
        raise typer.Exit(code=2)
    # Accept either a bare list or {"verdicts": [...]}.
    pairs = data.get("verdicts", []) if isinstance(data, dict) else data
    if not isinstance(pairs, list):
        typer.echo("Error: verdicts must be a JSON list of {winner, loser}", err=True)
        raise typer.Exit(code=2)

    entries = read_graph(_graph_path())
    by_id = {e.get("id"): e for e in entries if isinstance(e.get("id"), str)}

    # Participants are verdict ids that actually exist in the graph. Dropping
    # unknown ids (an LLM typo or a stale node) mirrors the validate path and
    # keeps the emitted order applyable: a non-graph id would survive to the
    # output with a null title and then fail `fno backlog rank`.
    ids: list[str] = []
    for v in pairs:
        if isinstance(v, dict):
            for k in ("winner", "loser"):
                val = v.get(k)
                if isinstance(val, str) and val in by_id:
                    ids.append(val)

    meta = {
        i: (
            PRIORITY_ORDER.get(by_id.get(i, {}).get("priority", "p2"), 2),
            by_id.get(i, {}).get("created_at", "") or "",
        )
        for i in set(ids)
    }
    ranked = _copeland_rank(ids, pairs, meta)
    for r in ranked:
        r["title"] = by_id.get(r["id"], {}).get("title")
    typer.echo(json.dumps({"order": ranked}, indent=2))


@cli.command("validate")
def cmd_validate(
    proposal: Path = typer.Argument(..., help="Path to proposal.json"),
) -> None:
    """Validate a proposal, drop cycles and unknown-id entries, print cleaned JSON."""
    from fno.graph.store import read_graph

    data = _load_proposal(proposal)
    entries = read_graph(_graph_path())
    cleaned, errors = _validate_proposal(data, entries)
    for err in errors:
        typer.echo(err, err=True)
    cleaned["validation_errors"] = errors
    typer.echo(json.dumps(cleaned, indent=2))
    if errors:
        raise typer.Exit(code=3)


@cli.command("apply")
def cmd_apply(
    proposal: Path = typer.Argument(..., help="Path to proposal.json"),
    pick: Optional[str] = typer.Option(
        None,
        "--pick",
        help="Comma-separated edge keys / IDs to apply as a subset",
    ),
) -> None:
    """Apply a validated proposal to the graph under a single locked mutation."""
    from fno.graph.store import locked_mutate_graph

    data = _load_proposal(proposal)

    pick_ids: Optional[set[str]] = None
    if pick:
        pick_ids = set(pick.split(","))

    def _filter_pick(cleaned: dict) -> dict:
        if pick_ids is None:
            return cleaned
        return {
            "dependencies": [
                d for d in cleaned["dependencies"]
                if f"{d['from']}->{d['to']}" in pick_ids
            ],
            "priority_changes": [
                p for p in cleaned["priority_changes"]
                if p.get("id") in pick_ids
            ],
            "duplicates": [
                d for d in cleaned["duplicates"]
                if ",".join(d.get("ids", [])) in pick_ids
            ],
            "defer": [
                d for d in cleaned.get("defer", [])
                if d.get("id") in pick_ids
            ],
        }

    applied = {
        "dependencies": 0,
        "priority_changes": 0,
        "duplicates_flagged": 0,
        "deferred": 0,
    }
    priority_moves: list[dict] = []
    locked_errors_holder: list[list[str]] = [[]]

    def mutator(entries: list[dict]) -> list[dict]:
        # Re-validate against the locked snapshot so TOCTOU races (another
        # writer adding edges between load and apply) can't sneak cycles in.
        cleaned_locked, errors_locked = _validate_proposal(data, entries)
        cleaned_locked = _filter_pick(cleaned_locked)
        locked_errors_holder[0] = errors_locked

        by_id = {e.get("id"): e for e in entries if isinstance(e.get("id"), str)}
        for edge in cleaned_locked["dependencies"]:
            target = by_id.get(edge["to"])
            if target is None:
                continue
            existing = set(target.get("blocked_by", []))
            if edge["from"] not in existing:
                target.setdefault("blocked_by", []).append(edge["from"])
                applied["dependencies"] += 1
        for pc in cleaned_locked["priority_changes"]:
            node = by_id.get(pc["id"])
            if node is None:
                continue
            priority_moves.append(
                {"id": pc["id"], "from": node.get("priority"), "to": pc["to"]}
            )
            node["priority"] = pc["to"]
            applied["priority_changes"] += 1
        # Defer entries land deferred_at + deferred_reason on each target.
        # The cascade in recompute_statuses derives status: deferred from
        # deferred_at after the locked mutation completes.
        from datetime import datetime, timezone
        for d in cleaned_locked.get("defer", []):
            node = by_id.get(d["id"])
            if node is None:
                continue
            # Clear completed_at so the deferred cascade can take effect.
            # Without this, deferring an already-done node would keep the
            # row pinned to status: done because of the `done > deferred`
            # precedence in recompute_statuses. Symmetric with the direct
            # cmd_defer verb (cli.py).
            node["completed_at"] = None
            node["deferred_at"] = datetime.now(timezone.utc).isoformat()
            node["deferred_reason"] = d["reason"]
            # Clear the canonical lock field; _normalize_lock_fields re-syncs the
            # session_id mirror and clears the harness stamp at serialize.
            node["locked_by"] = None
            node["claimed_at"] = None
            applied["deferred"] += 1
        applied["duplicates_flagged"] = len(cleaned_locked["duplicates"])
        return entries

    locked_mutate_graph(_graph_path(), mutator)

    # Telemetry (x-64cb US2): the mutation has committed; emit is best-effort and
    # must precede the Exit(3) below so a partial apply still records what landed.
    # proposed is the raw entry count across every category (the drop-rate
    # denominator); dropped is what _validate_proposal rejected.
    proposed = sum(
        len(data.get(k, []) or [])
        for k in ("dependencies", "priority_changes", "duplicates", "defer")
    )
    _emit_triage_applied(
        applied, priority_moves, proposed, len(locked_errors_holder[0])
    )

    for err in locked_errors_holder[0]:
        typer.echo(err, err=True)

    typer.echo(
        json.dumps(
            {
                "applied": applied,
                "dropped_due_to_validation": len(locked_errors_holder[0]),
            },
            indent=2,
        )
    )

    # Mirror cmd_validate: a non-empty error list signals partial application
    # and exits 3. A scripted caller (triage skill, run-loop) can then detect
    # that the proposal didn't fully land and decide whether to retry, log,
    # or abort. Without this, validate and apply diverge on exit semantics
    # and silent partial application slips through.
    if locked_errors_holder[0]:
        raise typer.Exit(code=3)


@cli.command("projects")
def cmd_projects(
    roadmap_id: Optional[str] = typer.Option(
        None, "--roadmap-id", help="Filter by roadmap ID"
    ),
) -> None:
    """List projects that have at least one pending node (alphabetical).

    Shape: ``{"projects": [{"name": str, "pending_count": int}, ...]}``.
    The ``each`` iteration mode in the /triage skill reads ``pending_count``
    for its per-project banner, and the legacy ``scripts/triage.py projects``
    integration test locks this shape in; both would break if this were
    flattened to a bare list of names.
    """
    from fno.graph.store import read_graph

    entries = read_graph(_graph_path())
    counts: dict[str, int] = {}
    for e in entries:
        if roadmap_id and e.get("roadmap_id") != roadmap_id:
            continue
        if not _is_pending(e):
            continue
        proj = e.get("project")
        if not proj:
            # Legacy entries without a project field would produce an
            # unroutable triage pass; skip rather than grouping under a
            # sentinel bucket.
            continue
        counts[proj] = counts.get(proj, 0) + 1
    out = [{"name": name, "pending_count": n} for name, n in sorted(counts.items())]
    typer.echo(json.dumps({"projects": out}, indent=2))


# ---------------------------------------------------------------------------
# pile: the triage pile - deferred nodes with their reason (G2, x-3236)
# ---------------------------------------------------------------------------


@cli.command("pile", hidden=True)
def cmd_pile(
    project: Optional[str] = typer.Option(None, "--project", "-p"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """The triage pile: deferred nodes with their reason, oldest-first (G2).

    No new lifecycle state (epic LD5) - the pile IS ``deferred`` +
    ``deferred_reason``. Surfaces what selection quarantined or a human paused so
    nothing rots invisibly. Hidden verb (menu-caps); the sole authority is the
    derived ``status``, so a node undeferred out of band drops off immediately.
    """
    from datetime import datetime, timezone

    from fno.graph.store import read_graph
    from fno.graph.statuses import recompute_statuses
    from fno.graph.maintain import _parse_ts

    entries = recompute_statuses(read_graph(_graph_path()))
    now = datetime.now(timezone.utc)
    rows: list[dict] = []
    for e in entries:
        if e.get("status") != "deferred":
            continue
        if project and e.get("project") != project:
            continue
        dt = _parse_ts(e.get("deferred_at"))
        age_days = (now - dt).days if dt is not None else None
        rows.append({
            "id": e.get("id"),
            "slug": e.get("slug"),
            "title": e.get("title"),
            "reason": e.get("deferred_reason"),
            "age_days": age_days,
            "priority": e.get("priority"),
            "project": e.get("project"),
        })
    # Oldest-first: longest-deferred leads; unknown-age rows sort last.
    rows.sort(key=lambda r: (r["age_days"] is None, -(r["age_days"] or 0)))

    if json_output:
        typer.echo(json.dumps({"pile": rows}, indent=2))
        return
    if not rows:
        typer.echo("triage pile empty (no deferred nodes)")
        return
    for r in rows:
        age = f"{r['age_days']}d" if r["age_days"] is not None else "?"
        typer.echo(
            f"{r['id']}  {age:>5}  {r['priority'] or 'p?'}  "
            f"{r['project'] or '-'}  {r['reason'] or '(no reason)'}"
        )


# ---------------------------------------------------------------------------
# health: aggregate "is the backlog healthy?" metrics
# ---------------------------------------------------------------------------


@cli.command("health")
def cmd_health(
    project: Optional[str] = typer.Option(None, "--project"),
    all_projects: bool = typer.Option(False, "--all", "-A"),
    json_output: bool = typer.Option(False, "--json", "-J"),
    stale_days: int = typer.Option(
        30, "--stale-days", help="A ready node older than this counts as stale"
    ),
    check: bool = typer.Option(
        False,
        "--check",
        help="Evaluate thresholds against the report; exit 4 on breach. Loop-safe.",
    ),
    quiet: bool = typer.Option(
        False,
        "--quiet",
        help="Suppress stdout when --check finds no breach (cron/loop-safe).",
    ),
) -> None:
    """Aggregate backlog health: collisions, stale, failure-prone, idea pile.

    The collisions section runs an all-pairs file-overlap check across pending
    plans and deduplicates so each pair appears once. Severity defaults to
    medium-and-up so low-signal single-file overlaps do not flood the report.

    Acknowledged-but-now-resolved collisions are surfaced separately so you
    can verify a deliberately-accepted conflict resolved cleanly.
    """
    import sys
    from datetime import datetime, timezone
    from pathlib import Path as _Path

    from fno.graph._intake import filter_by_project
    from fno.graph.collision import (
        find_collisions,
        find_acknowledged_collisions,
        _find_repo_root,
        _resolve_plan_path,
    )
    from fno.graph.store import read_graph

    all_entries = read_graph(_graph_path())
    entries = filter_by_project(all_entries, project, all_projects)

    pending = [e for e in entries if _is_pending(e) or _is_idea(e)]
    pending_active = [e for e in entries if _is_pending(e)]

    # 1. Idea pile depth
    idea_count = sum(1 for e in pending if _is_idea(e))

    # 2. Stale ready nodes
    now = datetime.now(timezone.utc)
    stale: list[dict] = []
    for e in pending_active:
        if e.get("status") != "ready":
            continue
        created = e.get("created_at")
        if not created:
            continue
        try:
            created_dt = datetime.fromisoformat(str(created).replace("Z", "+00:00"))
        except ValueError:
            # A malformed created_at could be the exact node a user wants
            # flagged as stale; surface it on stderr rather than dropping
            # silently.
            print(
                f"Warning: cannot parse created_at on {e['id']}: {created!r}",
                file=sys.stderr,
            )
            continue
        age_days = (now - created_dt).days
        if age_days > stale_days:
            stale.append({"id": e["id"], "title": e.get("title", ""), "age_days": age_days})

    # 3. Failure-prone nodes: multi-attempt, no PR. The "multi-attempt"
    # threshold is configurable via config.health_monitor.thresholds.
    # failure_prone_attempts (defaults to 2). Load lazily so this verb
    # stays usable even when health_monitor is unavailable.
    try:
        from fno.health_monitor import load_config as _load_hm_config
        _hm_thresh = (_load_hm_config().get("thresholds") or {})
        _fp_min = int(_hm_thresh.get("failure_prone_attempts", 2))
        if _fp_min < 1:
            _fp_min = 2
    except (ImportError, TypeError, ValueError, AttributeError):
        # ImportError: module unavailable; TypeError/ValueError: malformed
        # config value; AttributeError: load_config returned a non-dict
        # (defensive - would also indicate a programmer error worth seeing).
        _fp_min = 2
    failure_prone: list[dict] = []
    for e in pending_active:
        sessions = e.get("cost_sessions") or []
        if len(sessions) >= _fp_min and not e.get("pr_number"):
            burned = sum(float(s.get("cost_usd") or 0) for s in sessions if isinstance(s, dict))
            failure_prone.append(
                {
                    "id": e["id"],
                    "title": e.get("title", ""),
                    "attempts": len(sessions),
                    "burned_usd": round(burned, 2),
                }
            )

    # 4. All-pairs collision check (medium+ only)
    # Resolve the candidate plan_path against the repo root before passing
    # it in. Without this, running `triage health` from a non-repo-root
    # directory would silently produce false negatives because relative
    # `plan_path` strings would not resolve from CWD. find_collisions
    # already resolves the OTHER plans' paths via the same helpers; the
    # candidate path is the caller's responsibility.
    repo_root = _find_repo_root()
    collisions: list[dict] = []
    seen_pairs: set[tuple[str, str]] = set()
    for e in pending_active:
        plan_path = e.get("plan_path")
        if not plan_path:
            continue
        resolved_path = _resolve_plan_path(plan_path, repo_root)
        node_collisions = find_collisions(resolved_path, entries, self_id=e["id"])
        for c in node_collisions:
            pair = tuple(sorted([e["id"], c.with_node_id]))
            if pair in seen_pairs:
                continue
            seen_pairs.add(pair)
            if c.severity in ("medium", "high"):
                collisions.append(
                    {
                        "between": list(pair),
                        "shared_files": c.shared_files,
                        "severity": c.severity,
                        "recommended_action": c.recommended_action,
                    }
                )

    # 5. Acknowledged-resolved nudges
    resolved = find_acknowledged_collisions(entries)
    resolved_payload = [
        {
            "node_id": r.node_id,
            "node_title": r.node_title,
            "resolved_via": r.resolved_via,
            "resolved_via_title": r.resolved_via_title,
            "resolved_via_status": r.resolved_via_status,
        }
        for r in resolved
    ]

    # 6. project<->cwd mismatch (pending-only, mapped-projects-only)
    from fno.graph._intake import project_root_from_settings
    _root_cache: dict[str, str | None] = {}
    mismatch_ids: list[str] = []
    for e in pending_active:
        proj = e.get("project")
        if not proj:
            continue
        if proj not in _root_cache:
            _root_cache[proj] = project_root_from_settings(proj)
        root = _root_cache[proj]
        if root is None:
            continue  # unmapped = work-map has no opinion; never counted
        raw_cwd = e.get("cwd")
        normalized = os.path.abspath(os.path.expanduser(str(raw_cwd))) if raw_cwd else None
        if normalized != root:
            mismatch_ids.append(e["id"])

    # 7. Stranded-by-failed-blocker (#34): dependents of an auto-failure-deferred
    # node that are now unreachable until the blocker is fixed/undeferred. Always
    # runs (read-only); an empty list means "none stranded", not "not checked".
    # Dependents are NEVER mutated here - surfacing only (Locked Decision #2).
    from fno.graph.failure import stranded_dependents as _stranded_dependents

    by_id = {
        e["id"]: e for e in entries if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    stranded_payload: list[dict] = []
    for blocker_id, dep_ids in _stranded_dependents(entries).items():
        blocker = by_id.get(blocker_id, {})
        stranded_payload.append(
            {
                "blocker": blocker_id,
                "blocker_title": blocker.get("title", ""),
                "deferred_reason": blocker.get("deferred_reason", ""),
                "dependents": [
                    {
                        "id": d,
                        "title": by_id.get(d, {}).get("title", ""),
                        "status": by_id.get(d, {}).get("status"),
                    }
                    for d in dep_ids
                ],
            }
        )

    # 8. Batch-lane verdict (advisory, best-effort): surfaced only when batch
    # ship/abandon events exist and the measured verdict says act. Never gates
    # the health exit code.
    batch_verdict: str | None = None
    try:
        from fno.backlog.batch import compute_metrics, read_batch_events

        # Batch state (and its journal) is canonical-rooted, like fno.claims;
        # the collision repo_root above is the working checkout, which differs
        # inside a linked worktree. When the health scope is a --project with a
        # mapped root, read THAT repo's journal (a controller checkout scoped
        # to another project must not report its own repo's batching) - codex
        # P2 on this PR. Unmapped/absent project falls back to ambient.
        _events_root = None
        if project:
            _proj_root = _root_cache.get(project, project_root_from_settings(project))
            if _proj_root:
                _events_root = _Path(_proj_root)
        if _events_root is None:
            try:
                from fno.paths import resolve_canonical_repo_root

                _events_root = resolve_canonical_repo_root()
            except Exception:  # noqa: BLE001 - outside a git repo, fall back to cwd
                _events_root = _Path.cwd()
        _batch_events = read_batch_events(_events_root / ".fno" / "events.jsonl")
        if _batch_events:
            _bv = compute_metrics(_batch_events)["verdict"]
            if _bv in ("build-wave4", "disable-batching"):
                batch_verdict = _bv
    except Exception:  # noqa: BLE001 - advisory only; health must not break
        pass

    # 9. Evals health (advisory, best-effort): regression pass rate + flake
    # count, shown only when eval history exists. This is the consumption armor
    # for the evals harness - the report has a wired consumer from day one.
    # Never gates the health exit code.
    evals_summary: dict | None = None
    try:
        from fno.evals.report import evals_health_summary
        from fno.paths import evals_history as _evals_history

        evals_summary = evals_health_summary(_evals_history())
    except Exception:  # noqa: BLE001 - advisory only; health must not break
        pass

    # 10. Routing + triage decision metrics (x-64cb): folded from the canonical
    # events log, event-gated (absent when no decisions recorded - AC6-EDGE).
    # Advisory only; a read failure leaves the sections off, never breaks health.
    routing_metrics: Optional[dict] = None
    # Orphan feature rate: open features that resolve no mission edge. Scoped to
    # `pending` like the other actionable metrics - a shipped orphan is history,
    # not work an operator can still roll up. The parent chain is walked against
    # the FULL graph, so a parent epic that is already done still counts as a
    # mission edge. Advisory: a rollup failure leaves the metric absent, never
    # breaks health.
    orphan_rate: Optional[float] = None
    orphan_nodes: list[str] = []
    try:
        from fno.graph.rollup import CLOSED_STATUSES, ROLLUP_TYPES, is_orphan

        # Ancestry resolves against the UNFILTERED graph: a project-B feature
        # may legitimately hang off a project-A epic (cross-project decompose),
        # and walking a project-scoped slice would report it as an orphan.
        # The numerator/denominator stay project-scoped; only the walk is global.
        index = {
            e["id"]: e for e in all_entries
            if isinstance(e, dict) and isinstance(e.get("id"), str)
        }
        # Every OPEN feature, not `pending`: that excludes claimed and in_review,
        # so claiming an orphan or opening its PR would drop the rate without a
        # single orphan being resolved.
        non_exempt = [
            e for e in entries
            if isinstance(e, dict)
            and e.get("type") in ROLLUP_TYPES
            and not e.get("orphan_ok")
            and e.get("status") not in CLOSED_STATUSES
        ]
        orphan_nodes = [
            nid for e in non_exempt
            if isinstance((nid := e.get("id")), str) and is_orphan(e, index)
        ]
        # Zero non-exempt features (greenfield) reads 0.0, never a ZeroDivision.
        orphan_rate = (
            round(len(orphan_nodes) / len(non_exempt), 4) if non_exempt else 0.0
        )
    except Exception:  # noqa: BLE001 - advisory only; health must not break
        pass

    triage_metrics: Optional[dict] = None
    try:
        _canon_events = _read_canonical_events()
        routing_metrics = fold_routing_health(_canon_events)
        triage_metrics = fold_triage_health(_canon_events)
    except Exception:  # noqa: BLE001 - advisory only; health must not break
        pass

    # done=merged invariant. Advisory on any failure: a check that crashes the
    # whole health report is a check that gets removed.
    try:
        dnm = done_not_merged_report(entries)
    except Exception:  # noqa: BLE001
        dnm = {"violations": [], "unknown": [], "checked": 0, "window_days": 0}

    report = {
        "scope": _resolve_scope(project, all_projects, entries),
        **({"routing": routing_metrics} if routing_metrics else {}),
        **({"triage_metrics": triage_metrics} if triage_metrics else {}),
        "idea_pile_depth": idea_count,
        "stale_ready_nodes": stale,
        "failure_prone_nodes": failure_prone,
        "collisions": collisions,
        "acknowledged_resolved": resolved_payload,
        "project_cwd_mismatch": len(mismatch_ids),
        "project_cwd_mismatch_nodes": mismatch_ids,
        **({"orphan_feature_rate": orphan_rate} if orphan_rate is not None else {}),
        **({"orphan_feature_nodes": orphan_nodes} if orphan_rate is not None else {}),
        "done_not_merged": dnm["violations"],
        "done_not_merged_unknown": dnm["unknown"],
        "stranded_by_failed_blocker": stranded_payload,
        **({"batch_verdict": batch_verdict} if batch_verdict else {}),
        **({"evals": evals_summary} if evals_summary else {}),
        "totals": {
            "pending": len(pending_active),
            "ideas": idea_count,
            "stale": len(stale),
            "failure_prone": len(failure_prone),
            "collisions": len(collisions),
            "acknowledged_resolved": len(resolved_payload),
            "project_cwd_mismatch": len(mismatch_ids),
            "stranded_by_failed_blocker": sum(
                len(s["dependents"]) for s in stranded_payload
            ),
        },
    }

    if quiet and not check:
        typer.echo(
            "Note: --quiet has no effect without --check (this command is "
            "always silent in healthy state when --check is set).",
            err=True,
        )

    if check:
        # --check flips exit-code semantics: 0 healthy, 4 breach. Honors
        # config thresholds, dispatches notifications, appends history.
        from fno.health_monitor import (
            evaluate_thresholds,
            dispatch_notifications,
            append_history,
            load_config,
        )

        hm_config = load_config()
        breaches = evaluate_thresholds(report, config=hm_config)
        history_cfg = hm_config.get("history") or {}
        if history_cfg.get("enabled", True):
            # `dict.get(k, default)` returns None when k is present-with-None
            # in YAML; the `or` fallback covers that case so Path(None)
            # never raises TypeError. Same shape for retain_days.
            history_path_str = (
                history_cfg.get("path")
                or str(_paths.state_dir() / "health-history.jsonl")
            )
            try:
                retain_days = int(history_cfg.get("retain_days", 90))
            except (TypeError, ValueError):
                retain_days = 90
            append_history(
                report,
                breaches,
                history_path=Path(history_path_str).expanduser(),
                retain_days=retain_days,
            )
        if breaches:
            dispatch_notifications(report, breaches, config=hm_config)
            if not quiet:
                if json_output:
                    typer.echo(
                        json.dumps(
                            {
                                "status": "breach",
                                "report": report,
                                "breaches": [b.to_jsonable() for b in breaches],
                            },
                            indent=2,
                        )
                    )
                else:
                    typer.echo("Backlog health: BREACH")
                    for b in breaches:
                        typer.echo(
                            f"  [{b.severity.upper()}] {b.key}: actual={b.actual} "
                            f"threshold={b.threshold}"
                        )
            raise typer.Exit(code=4)
        # healthy
        if not quiet:
            if json_output:
                typer.echo(
                    json.dumps({"status": "healthy", "report": report}, indent=2)
                )
            else:
                typer.echo(
                    f"Backlog health: OK (no thresholds breached) - {report['scope']}"
                )
        return

    if json_output:
        typer.echo(json.dumps(report, indent=2))
        return

    typer.echo(f"Backlog health: {report['scope']}")
    typer.echo(f"  pending: {report['totals']['pending']}")
    typer.echo(f"  ideas: {report['idea_pile_depth']}")
    typer.echo(f"  stale (>{stale_days}d ready): {report['totals']['stale']}")
    typer.echo(f"  failure-prone (>1 attempt, no PR): {report['totals']['failure_prone']}")
    typer.echo(f"  collisions (medium+): {report['totals']['collisions']}")
    typer.echo(f"  acknowledged-resolved: {report['totals']['acknowledged_resolved']}")
    typer.echo(f"  project<->cwd mismatches: {report['totals']['project_cwd_mismatch']}")
    if "orphan_feature_rate" in report:
        n_orphans = len(report.get("orphan_feature_nodes", []))
        typer.echo(
            f"  orphan features (no mission edge): "
            f"{report['orphan_feature_rate']:.0%} ({n_orphans})"
        )
    typer.echo(
        f"  stranded by failed blocker: "
        f"{report['totals']['stranded_by_failed_blocker']}"
    )
    if evals_summary:
        rate = evals_summary["regression_pass_rate"]
        alarm = " ALARM" if evals_summary["regression_alarm"] else ""
        rate_txt = f"regression pass {rate:.0%}, " if rate is not None else ""
        typer.echo(f"  evals: {rate_txt}flakes {evals_summary['flake_count']}{alarm}")
    if routing_metrics:
        rm = routing_metrics
        total = rm["total"]
        typer.echo("")
        typer.echo(f"Executor routing ({total} resolutions):")
        dist = ", ".join(
            f"{t}={c}/{total}" for t, c in sorted(rm["tier_distribution"].items())
        )
        typer.echo(f"  tier distribution: {dist}")
        typer.echo(f"  inference share: {rm['inference']}/{total}")
        typer.echo(f"  warn-fallback: {rm['warn_fallback_count']}/{total}")
        if rm["inferred_tasks"]:
            typer.echo(
                f"  override-after-inference (mis-route proxy): "
                f"{rm['overridden_after_inference']}/{rm['inferred_tasks']}"
            )
        else:
            typer.echo(
                "  override-after-inference (mis-route proxy): n/a "
                "(no inference-resolved tasks with ids)"
            )
    if triage_metrics:
        tm = triage_metrics
        cats = tm["applied_by_category"]
        typer.echo("")
        typer.echo(f"Triage applies ({tm['applies']}):")
        typer.echo(
            "  applied: "
            f"priority={cats['priority_changes']}, deps={cats['dependencies']}, "
            f"dups={cats['duplicates_flagged']}, deferred={cats['deferred']}"
        )
        if tm["proposed"]:
            typer.echo(
                f"  validation drop rate: {tm['dropped']}/{tm['proposed']}"
            )
        else:
            typer.echo("  validation drop rate: n/a (no proposal entries recorded)")
    if batch_verdict == "build-wave4":
        typer.echo(
            "  batch-lane verdict: build-wave4 - abandonment waste exceeds savings; "
            "consider building batch-lane Wave 4 (surgical isolation). "
            "See `fno backlog batch metrics`."
        )
    elif batch_verdict == "disable-batching":
        typer.echo(
            "  batch-lane verdict: disable-batching - batching costs more CI than it "
            "saves; consider config.batch.enabled: false. See `fno backlog batch metrics`."
        )
    if collisions:
        typer.echo("")
        typer.echo("Plans stepping on each other:")
        for c in collisions:
            typer.echo(
                f"  [{c['severity']}] {' <-> '.join(c['between'])}: "
                f"{len(c['shared_files'])} shared files; recommend {c['recommended_action']}"
            )
    if resolved_payload:
        typer.echo("")
        typer.echo("Acknowledged collisions now resolved (verify cleanup):")
        for r in resolved_payload:
            typer.echo(
                f"  {r['node_id']} acknowledged collision with {r['resolved_via']} "
                f"({r['resolved_via_status']}); review whether the conflict resolved cleanly."
            )
    if mismatch_ids:
        typer.echo("")
        typer.echo("Pending nodes with project<->cwd mismatch (producer regression):")
        for node_id in mismatch_ids:
            typer.echo(f"  {node_id}")
    if stranded_payload:
        typer.echo("")
        typer.echo("Stranded by a failed blocker (recover via undefer or fix the blocker):")
        for s in stranded_payload:
            typer.echo(f"  {s['blocker']} deferred ({s['deferred_reason']}) strands:")
            for d in s["dependents"]:
                typer.echo(f"    - {d['id']} [{d['status']}] {d['title']}")
    if dnm["violations"]:
        typer.echo("")
        typer.echo(
            f"Closed over an unmerged PR (last {dnm['window_days']}d) - "
            f"the map claims work that has not landed:"
        )
        for v in dnm["violations"]:
            typer.echo(
                f"  {v['id']} PR #{v['pr_number']} is {v['pr_state']}, "
                f"closed {v['completed_at']}  {v['title']}"
            )
    if dnm["unknown"]:
        typer.echo("")
        typer.echo(f"done=merged unknown (not a violation): {len(dnm['unknown'])} node(s)")


# ---------------------------------------------------------------------------
# trend: rolling-window readout of historical health checks
# ---------------------------------------------------------------------------


@cli.command("trend")
def cmd_trend(
    days: int = typer.Option(7, "--days", help="Window in days (default 7)"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Print a backlog-trend summary from health-check history.

    Reads ``~/.fno/health-history.jsonl`` (or the path set via the
    ``FNO_HEALTH_HISTORY`` env var, used by tests). Emits a
    first-vs-latest delta per metric over the requested window.
    """
    import os

    from fno.health_monitor import read_history, summarize_trend

    override = os.environ.get("FNO_HEALTH_HISTORY")
    history_path = Path(override).expanduser() if override else None
    entries = read_history(history_path=history_path, days=days)
    summary = summarize_trend(entries)

    if json_output:
        typer.echo(json.dumps({"days": days, "entries": len(entries), "summary": summary}, indent=2))
        return

    if not entries:
        typer.echo(f"Backlog trend (last {days} days): no history yet.")
        return

    typer.echo(f"Backlog trend (last {days} days, {len(entries)} entries):")
    for key, stats in summary.items():
        delta = stats["delta"]
        sign = "+" if delta >= 0 else ""
        pct = stats["percent_change"]
        pct_str = f"{sign}{pct}%" if pct is not None else "n/a"
        typer.echo(
            f"  {key}: {stats['first']} -> {stats['latest']} ({sign}{delta}, {pct_str})"
        )
