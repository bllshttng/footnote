"""Pure fold: turn ledger rows + events + graph nodes into a scoreboard dict.

Read-only. The readers are deliberately tolerant: the ledger gets one retry on
a parse error (AC5-FR: mid-append race) before it becomes a hard broken-input
(AC5-ERR); the optional jsonl/graph sources skip a corrupt line rather than
crash the whole verb. The one rule the whole file exists to enforce: a rate is
never emitted without its coverage on the same dict, so a partial window is
never mistaken for a real trend.
"""

from __future__ import annotations

import json
import re
import time
from collections import Counter
from datetime import datetime, timedelta
from pathlib import Path

# termination_reason -> outcome class. The delivered-ship set is the explicit
# _SHIPPED_TERMINALS allowlist below; the wedge set is the stuck-terminal set.
# Everything else (Interrupted, delegated, NoWork, DoneAwaitingMerge, or no
# reason at all) is neither and lands in "other" so the spend split always
# reconciles to the window total.
_WEDGE_REASONS = frozenset({"NoProgress", "Budget", "Aborted"})

# Terminal reasons that count a node as DELIVERED for telemetry (cost
# attribution + shipped/surviving node sets). An explicit allowlist, NOT a
# `startswith("Done")` prefix match: DoneAwaitingMerge is a Done* terminal
# (the agent finished) but the PR is NOT merged and its CI is red pending a
# human merge past pre-existing main-red, so counting it as delivered would
# inflate autonomy/survival metrics before the work actually lands. DoneBatched
# stays (it delivers via the shared batch PR). This is intentionally looser than
# finalize.SHIP_REASONS (which gates plan stamp/graduate on DonePRGreen|
# DoneAdvisory only) - "delivered for telemetry" and "graduate the plan" differ.
_SHIPPED_TERMINALS = frozenset({"DonePRGreen", "DoneAdvisory", "DoneBatched"})


def _is_shipped_reason(termination_reason: str | None) -> bool:
    """True iff a terminal reason counts as a delivered node for telemetry.
    Non-string junk (a hand-edited/partial ledger row) is never a ship reason,
    and must not reach the frozenset membership test unhashable."""
    tr = termination_reason if isinstance(termination_reason, str) else ""
    return tr in _SHIPPED_TERMINALS


# A plan-only thread produces planning output and no build phase. The
# discriminator is the phase SET (or a quick-entry type), never the terminal
# reason: a build that wedged after planning carries a do/review/ship phase and
# must stay `unshipped`, so it can never launder itself as `planned`.
_PLAN_PHASES = frozenset({"think", "plan"})


def _is_planned_row(row: dict) -> bool:
    phases = row.get("phases_completed")
    if isinstance(phases, list) and phases and all(p in _PLAN_PHASES for p in phases):
        return True
    return row.get("type") in ("think", "plan")


# Survival follow-up window: a fix-node created within this many days of a
# node's ship counts against that node's survival.
_SURVIVAL_FOLLOWUP_DAYS = 14


class BrokenLedger(Exception):
    """Ledger failed to parse twice - a real corruption, not a mid-append race."""

    def __init__(self, path: str, offset: int, msg: str):
        self.path = path
        self.offset = offset
        self.msg = msg
        super().__init__(f"{path}: parse error at byte {offset}: {msg}")


def load_ledger_rows(path: Path, *, _retry: bool = True) -> list[dict]:
    """Load ledger.json rows. Retry once on a parse error (AC5-FR); a second
    failure raises BrokenLedger (AC5-ERR). Missing file = empty (fresh install)."""
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except FileNotFoundError:
        return []
    except json.JSONDecodeError as e:
        if _retry:
            time.sleep(0.1)  # ponytail: fixed backoff; the writer holds flock for ms
            return load_ledger_rows(path, _retry=False)
        raise BrokenLedger(str(path), e.pos, e.msg) from e
    rows = data.get("entries", data) if isinstance(data, dict) else data
    if not isinstance(rows, list):  # {"entries": null} etc. - valid JSON, junk shape
        return []
    return [r for r in rows if isinstance(r, dict)]


def read_jsonl_events(paths: list[Path], kinds: set[str]) -> list[dict]:
    """Best-effort jsonl reader: skip a corrupt/partial line rather than crash
    (a trailing partial line during append is expected, not an error)."""
    out: list[dict] = []
    for p in paths:
        p = Path(p)
        if not p.exists():
            continue
        try:
            with p.open(encoding="utf-8") as f:
                for line in f:
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        e = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if (e.get("kind") or e.get("type")) in kinds:
                        out.append(e)
        except OSError:
            continue
    return out


def read_graph_nodes(path: Path) -> list[dict]:
    """Best-effort graph read for the optional survival signal. A missing or
    unreadable graph is not fatal - survival just degrades to n/a."""
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError):  # missing / unreadable / non-utf8 / bad json
        return []
    nodes = data.get("entries", data.get("nodes", data)) if isinstance(data, dict) else data
    if isinstance(nodes, dict):
        nodes = list(nodes.values())
    if not isinstance(nodes, list):  # valid JSON, junk shape (null / scalar / {"entries": null})
        return []
    return [n for n in nodes if isinstance(n, dict)]


def _parse_ts(raw) -> datetime | None:
    """Parse an ISO timestamp to a naive LOCAL datetime - the one timeline the
    whole fold uses.

    The dominant source, the ledger's `completed`, is written naive-local
    (`datetime.now().isoformat()`), and `now` is `datetime.now()` (also naive
    local), so those are compared apples-to-apples. Aware timestamps (events
    carry a `...Z` / offset) are converted to local *before* their tzinfo is
    stripped, so an offset like `+02:00` is not silently mis-read - it lands on
    the same local timeline instead of being off by the offset."""
    if not isinstance(raw, str) or not raw:
        return None
    try:
        dt = datetime.fromisoformat(raw.replace("Z", "+00:00"))
    except ValueError:
        return None
    if dt.tzinfo is not None:
        dt = dt.astimezone().replace(tzinfo=None)  # -> naive local
    return dt


def _pct(n: int, d: int) -> int:
    return round(100 * n / d) if d else 0


def _num(v) -> float:
    """Coerce a possibly-malformed ledger cost to float; junk -> 0.0."""
    try:
        return float(v or 0.0)
    except (TypeError, ValueError):
        return 0.0


def _num_opt(v) -> float | None:
    """Nullable numeric coercion: a MISSING/None/junk value is None, not 0.0, so
    an unmeasurable metric (finalize records None when transcript cost extraction
    is unavailable) stays distinct from a measured zero and is excluded from
    distributions rather than faking a zero-token/zero-minute session."""
    if v is None or v == "":
        return None
    try:
        return float(v)
    except (TypeError, ValueError):
        return None


def build_scoreboard(
    rows: list[dict],
    touch_events: list[dict],
    graph_nodes: list[dict],
    *,
    since_days: int,
    now: datetime,
) -> dict:
    """Fold the three sources into a render-ready dict. Pure; no I/O."""
    cutoff = now - timedelta(days=since_days)

    def _in_window(ts_raw) -> bool:
        dt = _parse_ts(ts_raw)
        return dt is not None and cutoff <= dt <= now

    windowed = [r for r in rows if _in_window(r.get("completed"))]
    total = len(windowed)

    if total == 0:
        return {"state": "no_data", "since_days": since_days, "rows": 0}

    with_tr = sum(1 for r in windowed if r.get("termination_reason"))
    with_node = sum(1 for r in windowed if r.get("graph_node_id"))
    coverage = {
        "rows": total,
        "termination_reason_pct": _pct(with_tr, total),
        "node_linkage_pct": _pct(with_node, total),
    }

    stop_cause = dict(
        Counter(r["termination_reason"] for r in windowed if r.get("termination_reason"))
    )

    ship_cost = wedge_cost = other_cost = 0.0
    for r in windowed:
        cost = _num(r.get("cost_usd"))  # a malformed cost never crashes the fold
        tr = r.get("termination_reason")
        if _is_shipped_reason(tr):
            ship_cost += cost
        elif tr in _WEDGE_REASONS:
            wedge_cost += cost
        else:
            other_cost += cost
    spend = {
        "ship_terminal_usd": round(ship_cost, 2),
        "wedge_terminal_usd": round(wedge_cost, 2),
        "other_usd": round(other_cost, 2),
    }

    # Shipped nodes in window: distinct graph_node_id among Done* rows.
    ship_rows = [r for r in windowed if _is_shipped_reason(r.get("termination_reason"))]
    shipped_nodes = {r["graph_node_id"] for r in ship_rows if r.get("graph_node_id")}

    autonomy = _autonomy(touch_events, shipped_nodes, cutoff, now)
    survival = _survival(shipped_nodes, ship_rows, graph_nodes)

    full = coverage["termination_reason_pct"] == 100 and autonomy["available"] and survival["available"]
    return {
        "state": "full" if full else "partial",
        "since_days": since_days,
        "coverage": coverage,
        "stop_cause": stop_cause,
        "spend": spend,
        "autonomy": autonomy,
        "survival": survival,
    }


def _autonomy(touch_events: list[dict], shipped_nodes: set, cutoff, now) -> dict:
    """human_touch events per shipped node. Degrades to n/a until Wave 4 emits
    any human_touch event at all."""
    if not touch_events:
        return {"available": False, "reason": "no human_touch signals (Wave 4 not shipped)"}
    if not shipped_nodes:
        return {"available": False, "reason": "no shipped nodes in window"}
    in_window = [e for e in touch_events if _event_in_window(e, cutoff, now)]
    return {
        "available": True,
        "touches": len(in_window),
        "shipped_nodes": len(shipped_nodes),
        "touches_per_shipped_node": round(len(in_window) / len(shipped_nodes), 2),
    }


def _survival(shipped_nodes: set, ship_rows: list[dict], graph_nodes: list[dict]) -> dict:
    """Shipped nodes with no `reverted` flag and no caused_by fix-node created
    within the follow-up window. Degrades to n/a until any node carries a Wave 4
    causal field."""
    w4 = any(("reverted" in n) or n.get("caused_by") for n in graph_nodes)
    if not w4:
        return {"available": False, "reason": "no causal telemetry (Wave 4 not shipped)"}
    if not shipped_nodes:
        return {"available": False, "reason": "no shipped nodes in window"}

    by_id = {n.get("id"): n for n in graph_nodes if n.get("id")}
    ship_ts = {r["graph_node_id"]: _parse_ts(r.get("completed")) for r in ship_rows if r.get("graph_node_id")}
    # Fix-nodes grouped by the node they blame.
    fixes: dict[str, list[dict]] = {}
    for n in graph_nodes:
        origin = n.get("caused_by")
        if origin:
            fixes.setdefault(origin, []).append(n)

    survived = 0
    for nid in shipped_nodes:
        node = by_id.get(nid, {})
        if node.get("reverted"):
            continue
        shipped_at = ship_ts.get(nid)
        followed = False
        for fx in fixes.get(nid, []):
            fx_at = _parse_ts(fx.get("created_at"))
            # A follow-up is a fix created AFTER the ship, within the window. A
            # fix pre-dating the ship (negative delta) is not a follow-up to it.
            if shipped_at and fx_at and timedelta(0) <= (fx_at - shipped_at) <= timedelta(days=_SURVIVAL_FOLLOWUP_DAYS):
                followed = True
                break
            if not shipped_at or not fx_at:
                followed = True  # can't time-bound it; count conservatively against survival
                break
        if not followed:
            survived += 1

    n = len(shipped_nodes)
    return {"available": True, "survived": survived, "shipped_nodes": n, "rate_pct": _pct(survived, n)}


def _event_in_window(e: dict, cutoff, now) -> bool:
    ts_raw = e.get("ts")
    if not ts_raw:
        data = e.get("data")
        ts_raw = data.get("ts") if isinstance(data, dict) else None
    dt = _parse_ts(ts_raw)
    return dt is None or cutoff <= dt <= now  # undated events count in (best-effort)


# ── verifier calibration (W6 x-f063) ────────────────────────────────────────

_CALIBRATION_MIN_VERDICTS = 10
_COUNTABLE_VERDICTS = ("pass", "concerns", "fail")
_OUTCOMES = ("merged_clean", "bounced", "reverted")


def _node_outcome(nid: str, shipped_at, by_id: dict, fixes: dict) -> str:
    """Derive a shipped node's outcome from the W4/W5 signals: `reverted` flag,
    else a caused_by fix-node within the follow-up window -> bounced, else
    merged_clean. Un-time-boundable fixes count against the node (conservative,
    mirrors _survival)."""
    if by_id.get(nid, {}).get("reverted"):
        return "reverted"
    for fx in fixes.get(nid, []):
        fx_at = _parse_ts(fx.get("created_at"))
        if shipped_at and fx_at:
            if timedelta(0) <= (fx_at - shipped_at) <= timedelta(days=_SURVIVAL_FOLLOWUP_DAYS):
                return "bounced"
        else:
            return "bounced"
    return "merged_clean"


def build_calibration(verdict_events: list[dict], rows: list[dict], graph_nodes: list[dict]) -> dict:
    """Join verifier_verdict events to per-node outcomes. Pure; no I/O.

    Latest verdict per node wins (events arrive in append order). error /
    not_applicable finals are excluded from the table and reported as counts so
    the denominator stays honest; verdicts with no graph_node_id likewise. The
    table is gated on >= _CALIBRATION_MIN_VERDICTS countable verdicts (AC6-UI).
    """
    final: dict[str, str] = {}
    unattributed = 0
    for e in verdict_events:
        data = e.get("data") if isinstance(e.get("data"), dict) else {}
        v = data.get("verdict")
        if v not in _COUNTABLE_VERDICTS and v not in ("error", "not_applicable"):
            continue
        nid = data.get("graph_node_id")
        if not nid:
            unattributed += 1
            continue
        final[nid] = v  # append order: last write wins

    excluded = dict(Counter(v for v in final.values() if v not in _COUNTABLE_VERDICTS))
    counted = {nid: v for nid, v in final.items() if v in _COUNTABLE_VERDICTS}
    n = len(counted)
    base = {"n": n, "excluded": excluded, "unattributed": unattributed}
    if n < _CALIBRATION_MIN_VERDICTS:
        return {"state": "insufficient", "need": _CALIBRATION_MIN_VERDICTS, **base}

    by_id = {g.get("id"): g for g in graph_nodes if g.get("id")}
    fixes: dict[str, list[dict]] = {}
    for g in graph_nodes:
        origin = g.get("caused_by")
        if origin:
            fixes.setdefault(origin, []).append(g)
    ship_ts: dict[str, datetime] = {}
    for r in rows:
        nid = r.get("graph_node_id")
        if nid and _is_shipped_reason(r.get("termination_reason")):
            dt = _parse_ts(r.get("completed"))
            if dt and (nid not in ship_ts or dt > ship_ts[nid]):
                ship_ts[nid] = dt

    table = {v: {o: 0 for o in _OUTCOMES} for v in _COUNTABLE_VERDICTS}
    for nid, v in counted.items():
        table[v][_node_outcome(nid, ship_ts.get(nid), by_id, fixes)] += 1
    # Coverage-honesty (fold rule): a node with no timestamped Done-row gets a
    # conservative outcome (any caused_by fix counts as bounced, however old),
    # so surface how many outcomes were derived untimed rather than letting a
    # ledger gap silently bias the headline rate.
    untimed = sum(1 for nid in counted if nid not in ship_ts)
    # The rate the whole task exists to measure: verdict pass -> bad outcome.
    fp = table["pass"]["bounced"] + table["pass"]["reverted"]
    passes = sum(table["pass"].values())
    return {
        "state": "ok",
        **base,
        "untimed_outcomes": untimed,
        "table": table,
        "false_positive": {"count": fp, "of_pass": passes, "rate_pct": _pct(fp, passes)},
    }


# ── skill-outcome attribution (x-4829, loops roadmap W1) ────────────────────
#
# NO new events, NO new state files (telemetry locked rule) - this folds two
# things that already exist: the Skill tool_use blocks Claude Code already
# writes into ~/.claude/projects/*/<session>.jsonl transcripts (primary,
# real skill identity), and the ledger's own `phases_completed` list (v1
# proxy for the pipeline-driver skills, when a row carries no transcript-
# loadable session). A row with neither is an explicit "unattributed" bucket,
# never silently dropped (mirrors the calibration fold's honesty rule).

# think/plan/do/review/docs/ship/external are /target's own phase names;
# ship+external both route through /pr (create vs check) so they collapse to
# one skill id.
_PHASE_TO_SKILL = {
    "think": "fno:think",
    "plan": "fno:blueprint",
    "do": "fno:do",
    "review": "fno:review",
    "docs": "fno:ship-docs",
    "ship": "fno:pr",
    "external": "fno:pr",
}


def _skills_from_transcript(lines: list[str]) -> list[str]:
    """Scan transcript JSONL lines for Skill tool_use blocks; return the
    `input.skill` id of each (duplicates kept - callers dedupe if needed)."""
    found: list[str] = []
    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(obj, dict):  # valid JSON, junk shape (e.g. a bare scalar) - skip, don't crash
            continue
        message = obj.get("message")
        content = message.get("content") if isinstance(message, dict) else None
        if not isinstance(content, list):
            continue
        for block in content:
            if isinstance(block, dict) and block.get("type") == "tool_use" and block.get("name") == "Skill":
                input_ = block.get("input")
                skill = input_.get("skill") if isinstance(input_, dict) else None
                if skill:
                    found.append(skill)
    return found


def _default_read_transcript(session_id: str) -> list[str] | None:
    from fno.cost._session_cost import find_transcript

    path = find_transcript(session_id)
    if not path:
        return None
    try:
        return Path(path).read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return None


# Process-lifetime cache: skill git history is append-only, so fetching it
# once per (repo root, path) and bisecting in memory turns version resolution
# from O(N) git-log subprocess spawns (one per row) into O(S) (one per unique
# skill) - the ledger already holds thousands of rows (gemini + code-reviewer
# both flagged the naive per-row subprocess as a real latency risk).
_SKILL_COMMIT_HISTORY_CACHE: dict[tuple[str, str], list[tuple[datetime, str]]] = {}


def _skill_commit_history(root: Path, rel: str) -> list[tuple[datetime, str]]:
    """All commits touching rel, oldest first: [(commit datetime, short hash), ...].
    Empty on any git failure - never raises."""
    import subprocess

    key = (str(root), rel)
    if key in _SKILL_COMMIT_HISTORY_CACHE:
        return _SKILL_COMMIT_HISTORY_CACHE[key]
    history: list[tuple[datetime, str]] = []
    try:
        out = subprocess.run(
            ["git", "log", "--format=%h %aI", "--", rel],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=10,
        )
        if out.returncode == 0:
            for line in out.stdout.splitlines():
                h, _, iso = line.partition(" ")
                dt = _parse_ts(iso)
                if h and dt:
                    history.append((dt, h))
    except (OSError, subprocess.SubprocessError):
        pass
    history.sort(key=lambda pair: pair[0])
    _SKILL_COMMIT_HISTORY_CACHE[key] = history
    return history


def _default_skill_version(skill_id: str, ts_raw: str | None) -> str:
    """Best-effort git hash of the skill's SKILL.md at (or before) ts_raw.
    No git history / no such file / any subprocess failure -> "unknown",
    never an error (AC-EDGE)."""
    from fno.paths import resolve_repo_root

    name = skill_id.split(":", 1)[1] if ":" in skill_id else skill_id
    rel = f"skills/{name}/SKILL.md"
    try:
        root = resolve_repo_root()
    except Exception:
        return "unknown"
    if not (root / rel).exists():
        return "unknown"

    until_dt = _parse_ts(ts_raw) or datetime.now()
    best: str | None = None
    for dt, h in _skill_commit_history(root, rel):
        if dt > until_dt:
            break
        best = h
    return best or "unknown"


def _extract_skill_runs(row: dict, *, read_transcript) -> tuple[list[str], str]:
    """Return (distinct skill ids, method) for one ledger row.

    method is "transcript" (real Skill tool_use found), "phase-proxy"
    (fallback to phases_completed), or "unattributed" (neither)."""
    from fno.cost._session_cost import UUID_RE

    sessions = row.get("sessions")
    uuid_sessions = [s for s in sessions if isinstance(s, str) and UUID_RE.match(s)] if isinstance(sessions, list) else []
    found: list[str] = []
    for sid in uuid_sessions:
        lines = read_transcript(sid)
        if lines:
            found.extend(_skills_from_transcript(lines))
    if found:
        return sorted(set(found)), "transcript"

    phases = row.get("phases_completed")
    proxy = sorted({_PHASE_TO_SKILL[p] for p in phases if p in _PHASE_TO_SKILL}) if isinstance(phases, list) else []
    if proxy:
        return proxy, "phase-proxy"
    return [], "unattributed"


def _new_skill_bucket() -> dict:
    return {
        "runs": 0,
        "shipped": 0,
        "shipped_linked": 0,  # shipped AND has graph_node_id - the only rows revert_rate can judge
        "reverted": 0,
        "cost_total": 0.0,
        "touches_total": 0,
        "methods": set(),
    }


def build_skill_scoreboard(
    rows: list[dict],
    graph_nodes: list[dict],
    touch_events: list[dict],
    *,
    since_days: int,
    now: datetime,
    read_transcript=None,
    resolve_skill_version=None,
) -> dict:
    """Join session -> skill(s) -> node -> outcome/touches/cost. Pure except
    for the two injectable I/O hooks (transcript read, git-hash resolve),
    which default to the real implementations.

    Cost and touch counts are attributed in full to every skill found in a
    row (no fractional split across skills sharing one session) - a session
    that ran two skills counts fully toward both. ponytail: acceptable v1
    approximation; split later if a skill's numbers look inflated."""
    read_transcript = read_transcript or _default_read_transcript
    resolve_skill_version = resolve_skill_version or _default_skill_version

    cutoff = now - timedelta(days=since_days)

    def _in_window(ts_raw) -> bool:
        dt = _parse_ts(ts_raw)
        return dt is not None and cutoff <= dt <= now

    windowed = [r for r in rows if _in_window(r.get("completed"))]
    total = len(windowed)
    if total == 0:
        return {"state": "no_data", "since_days": since_days, "rows": 0}

    by_id = {n.get("id"): n for n in graph_nodes if n.get("id")}
    fixes: dict[str, list[dict]] = {}
    for n in graph_nodes:
        origin = n.get("caused_by")
        if origin:
            fixes.setdefault(origin, []).append(n)

    # W4 causal telemetry availability - mirrors _survival's own gate. Without
    # it, _node_outcome's "no fix found" branch is indistinguishable from
    # "revert data doesn't exist yet", so judging revert rate at all would
    # silently show a clean 0% where the real answer is "unknown" (codex peer
    # review finding).
    w4_available = any(("reverted" in n) or n.get("caused_by") for n in graph_nodes)

    touches_by_node: Counter = Counter()
    for e in touch_events:
        if not isinstance(e, dict):
            continue
        if not _event_in_window(e, cutoff, now):  # codex finding: touches must respect --since too
            continue
        nid = e.get("graph_node_id")
        if not nid:
            data = e.get("data")
            nid = data.get("graph_node_id") if isinstance(data, dict) else None
        if nid:
            touches_by_node[nid] += 1

    buckets: dict[tuple[str, str], dict] = {}
    attributed = 0

    for r in windowed:
        skills, method = _extract_skill_runs(r, read_transcript=read_transcript)
        shipped = _is_shipped_reason(r.get("termination_reason"))
        nid = r.get("graph_node_id")
        cost = _num(r.get("cost_usd"))
        touches = touches_by_node.get(nid, 0) if nid else 0

        if not skills:
            b = buckets.setdefault(("unattributed", "-"), _new_skill_bucket())
            b["runs"] += 1
            b["cost_total"] += cost
            b["touches_total"] += touches
            continue
        attributed += 1

        # Judgeable only when the node actually resolves in the graph AND the
        # graph carries causal telemetry at all - a resolvable-but-untracked
        # node must not silently default to "merged_clean" (same gap class as
        # _survival's w4 check, applied here per codex peer review).
        judgeable = bool(shipped and nid and w4_available and nid in by_id)
        outcome = _node_outcome(nid, _parse_ts(r.get("completed")), by_id, fixes) if judgeable else None

        for skill in skills:
            version = resolve_skill_version(skill, r.get("completed"))
            b = buckets.setdefault((skill, version), _new_skill_bucket())
            b["runs"] += 1
            b["cost_total"] += cost
            b["touches_total"] += touches
            b["methods"].add(method)
            if shipped:
                b["shipped"] += 1
                if judgeable:
                    b["shipped_linked"] += 1
                    if outcome in ("reverted", "bounced"):
                        b["reverted"] += 1

    out_rows = []
    for (skill, version), b in buckets.items():
        runs = b["runs"]
        out_rows.append(
            {
                "skill": skill,
                "version": version,
                "runs": runs,
                "ship_rate_pct": _pct(b["shipped"], runs),
                # None (not 0) when nothing was judgeable - a bare 0% would be
                # indistinguishable from "zero of N judged rows reverted", a
                # real and different fact (AC honesty; codex peer review).
                "revert_rate_pct": _pct(b["reverted"], b["shipped_linked"]) if b["shipped_linked"] else None,
                "touches_per_run": round(b["touches_total"] / runs, 2) if runs else 0.0,
                "cost_per_run": round(b["cost_total"] / runs, 2) if runs else 0.0,
                "method": "+".join(sorted(b["methods"])) if b["methods"] else "unattributed",
            }
        )
    out_rows.sort(key=lambda x: (-x["runs"], x["skill"]))

    return {
        "state": "ok",
        "since_days": since_days,
        "coverage": {"rows": total, "attributed_pct": _pct(attributed, total)},
        "rows": out_rows,
    }


# ── provider-outcome attribution (x-140c) ───────────────────────────────────
#
# What does a shipped PR cost on each provider/model, and whose work bounces
# most after shipping. One more group-by on the same fold: reuses
# _SHIPPED_TERMINALS, _node_outcome, and the coverage-honesty rule. The
# numerator deliberately includes wedge spend - a provider that wedges half
# its runs shows a proportionally higher cost per outcome, which is the
# signal quota-aware dispatch needs.


def build_provider_scoreboard(
    rows: list[dict],
    graph_nodes: list[dict],
    *,
    since_days: int,
    now: datetime,
) -> dict:
    """Group in-window execution rows by (provider_id, model) and attribute
    spend to outcomes. Pure; no I/O.

    Only `type == "execution"` rows count: the ledger's ~2k backfill entries
    carry session-scoped costs and would silently inflate both coverage and
    spend. Rows without provider_id land in a visible "unattributed" bucket,
    never dropped and never guessed from model."""
    cutoff = now - timedelta(days=since_days)

    def _in_window(ts_raw) -> bool:
        dt = _parse_ts(ts_raw)
        return dt is not None and cutoff <= dt <= now

    windowed = [r for r in rows if r.get("type") == "execution" and _in_window(r.get("completed"))]
    total = len(windowed)
    if total == 0:
        return {"state": "no_data", "since_days": since_days, "rows": 0}

    by_id = {n.get("id"): n for n in graph_nodes if n.get("id")}
    fixes: dict[str, list[dict]] = {}
    for n in graph_nodes:
        origin = n.get("caused_by")
        if origin:
            fixes.setdefault(origin, []).append(n)
    # Same W4 gate as _survival/build_skill_scoreboard: without causal
    # telemetry, "no fix found" is indistinguishable from "no revert data
    # yet", so bounce degrades to n/a instead of a fake 0%.
    w4_available = any(("reverted" in n) or n.get("caused_by") for n in graph_nodes)

    def _key(v, fallback: str) -> str:
        # Junk-tolerant like _num: a non-string provider/model never crashes
        # the fold, it lands in the fallback bucket.
        return v if isinstance(v, str) and v else fallback

    buckets: dict[tuple[str, str], dict] = {}
    attributed = 0
    for r in windowed:
        provider = _key(r.get("provider_id"), "unattributed")
        if provider != "unattributed":
            attributed += 1
        b = buckets.setdefault(
            (provider, _key(r.get("model"), "unknown")),
            {"runs": 0, "shipped": 0, "shipped_linked": 0, "bounced": 0,
             "spend": 0.0, "measured_cost": False, "iterations": [], "nids": set(), "nid_rows": 0},
        )
        b["runs"] += 1
        b["spend"] += _num(r.get("cost_usd"))
        if _num_opt(r.get("cost_usd")) is not None:  # a real recorded cost, not a coerced-missing 0
            b["measured_cost"] = True
        nid = r.get("graph_node_id")
        nid = nid if isinstance(nid, str) else None
        if nid:
            b["nids"].add(nid)
            b["nid_rows"] += 1
        if _is_shipped_reason(r.get("termination_reason")):
            b["shipped"] += 1
            it = _num_opt(r.get("iterations"))
            if it is not None:
                b["iterations"].append(it)
            if nid and w4_available and nid in by_id:
                b["shipped_linked"] += 1
                if _node_outcome(nid, _parse_ts(r.get("completed")), by_id, fixes) in ("bounced", "reverted"):
                    b["bounced"] += 1

    out_rows = []
    for (provider, model), b in buckets.items():
        out_rows.append(
            {
                "provider": provider,
                "model": model,
                "runs": b["runs"],
                "shipped": b["shipped"],
                "spend_usd": round(b["spend"], 2),
                # None (not $0.00) when nothing shipped OR the bucket recorded no
                # real cost - a missing-cost bucket is unmeasurable, not free, and
                # a fake $0/shipped would read as "cheapest" to quota dispatch.
                "cost_per_shipped_usd": round(b["spend"] / b["shipped"], 2) if b["shipped"] and b["measured_cost"] else None,
                "bounce_rate_pct": _pct(b["bounced"], b["shipped_linked"]) if b["shipped_linked"] else None,
                "shipped_linked": b["shipped_linked"],
                "median_iterations": _percentile(b["iterations"], 50),
                "retry_rows": b["nid_rows"] - len(b["nids"]),
            }
        )
    # Per-provider sub-rows stay contiguous for the renderer; unattributed last.
    out_rows.sort(key=lambda x: (x["provider"] == "unattributed", x["provider"], -x["spend_usd"], x["model"]))

    return {
        "state": "ok",
        "since_days": since_days,
        "coverage": {"rows": total, "attributed_pct": _pct(attributed, total)},
        "rows": out_rows,
    }


# ── session-efficiency fold (x-c284) ─────────────────────────────────────────
#
# Grades the PROCESS, not just the terminal state: a session that ships
# merged_clean but fired loop_check 132x and pushed red CI twice should read
# poorly on efficiency. Pure fold over telemetry that already exists (ledger row
# + loop_check events + graph), NO new events / state files (extends the x-4829
# locked rule). CI-red data comes from recorded loop_check `ci` values, never a
# gh call at fold time - the fold stays deterministic and offline.

# Recognized `ci` shapes. A FAILURE:* value is a red; the rest are recognized
# non-red states the emitter really produces (SUCCESS/PENDING/none plus the
# observed unknown/skipped). Anything else is emitter drift -> ci_unparsed, and
# the affected session's ci_reds degrades to None rather than a fabricated count.
# NOTE (plan deviation): the plan's recognized set was SUCCESS/FAILURE:*/PENDING/
# none, but real events.jsonl also carries `unknown` (1000s) and `skipped`
# (100s); treating those as drift would flag ci_reds=None on most sessions, so
# they are recognized-benign here. ci_unparsed still fires on a genuinely novel
# shape (AC6-FR).
_CI_RED_PREFIX = "FAILURE:"
_CI_RECOGNIZED_NONRED = frozenset({"SUCCESS", "PENDING", "none", "unknown", "skipped"})


def _percentile(values: list[float], p: int):
    """Nearest-rank percentile over a sorted copy. None on empty input. For a
    single value, every percentile is that value (no divide-by-zero)."""
    if not values:
        return None
    s = sorted(values)
    import math

    k = max(1, math.ceil(p / 100 * len(s)))
    return s[k - 1]


def _row_session_ids(row: dict) -> set[str]:
    """A row's session ids: the `sessions` list (both UUID and 20260708T-style),
    falling back to the scalar `session_id` for rows predating the list."""
    ids: set[str] = set()
    sessions = row.get("sessions")
    if isinstance(sessions, list):
        ids.update(s for s in sessions if isinstance(s, str) and s)
    scalar = row.get("session_id")
    if isinstance(scalar, str) and scalar:
        ids.add(scalar)
    return ids


def _ci_reds_from_fires(ci_ordered: list[str]) -> tuple[int | None, int]:
    """Count distinct red EPISODES (transitions into FAILURE:*) across a
    session's loop_check ci values in ts order. A red streak of N fires counts
    once. Returns (ci_reds, unparsed_count); ci_reds is None when any fire
    carried an unrecognized ci shape (drift must not be silently miscounted)."""
    unparsed = 0
    for ci in ci_ordered:
        if isinstance(ci, str) and (ci.startswith(_CI_RED_PREFIX) or ci in _CI_RECOGNIZED_NONRED):
            continue
        unparsed += 1
    if unparsed:
        return None, unparsed
    episodes = 0
    prev_red = False  # the pre-run state is non-red, so a leading FAILURE is one episode
    for ci in ci_ordered:
        is_red = isinstance(ci, str) and ci.startswith(_CI_RED_PREFIX)
        if is_red and not prev_red:
            episodes += 1
        prev_red = is_red
    return episodes, 0


def _transcript_counts(session_ids: set[str], read_transcript) -> tuple[int | None, int | None]:
    """Best-effort (turns, toolcalls) from a row's transcript(s): count message
    lines and tool_use blocks. Both None when no transcript resolves (GC'd /
    per-machine) - never 0, so unmeasurable stays distinct from measured-zero."""
    turns = 0
    toolcalls = 0
    found_any = False
    for sid in session_ids:
        lines = read_transcript(sid)
        if not lines:
            continue
        found_any = True
        for line in lines:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(obj, dict):
                continue
            if obj.get("message") is not None or obj.get("type") in ("user", "assistant"):
                turns += 1
            message = obj.get("message")
            content = message.get("content") if isinstance(message, dict) else None
            if isinstance(content, list):
                toolcalls += sum(1 for b in content if isinstance(b, dict) and b.get("type") == "tool_use")
    if not found_any:
        return None, None
    return turns, toolcalls


def _dist(rows_metric: list[float | int | None]) -> dict:
    """median/p90/n over the non-None values only (the denominator rides along)."""
    vals = [v for v in rows_metric if v is not None]
    return {"median": _percentile(vals, 50), "p90": _percentile(vals, 90), "n": len(vals)}


def build_efficiency(
    rows: list[dict],
    loop_events: list[dict],
    graph_nodes: list[dict],
    *,
    since_days: int,
    now: datetime,
    read_transcript=None,
) -> dict:
    """Fold ledger rows + loop_check events + graph into per-outcome-class costs
    and fleet distributions for session-efficiency. Pure except the injectable
    transcript hook (same seam as build_skill_scoreboard). Every rate/median
    carries its denominator in coverage - a partial window is never a trend."""
    read_transcript = read_transcript or _default_read_transcript
    cutoff = now - timedelta(days=since_days)

    def _in_window(ts_raw) -> bool:
        dt = _parse_ts(ts_raw)
        return dt is not None and cutoff <= dt <= now

    windowed = [r for r in rows if _in_window(r.get("completed"))]
    total = len(windowed)
    if total == 0:
        return {"state": "no_data", "since_days": since_days, "rows": 0}

    # Index loop_check fires by session id: (ts_sort_key, ci) so a session's
    # fires order deterministically even when ts is missing (undated last).
    fires_by_session: dict[str, list[tuple]] = {}
    for e in loop_events:
        if not isinstance(e, dict):
            continue
        data = e.get("data") if isinstance(e.get("data"), dict) else {}
        sid = data.get("session_id")
        if not isinstance(sid, str) or not sid:
            continue
        dt = _parse_ts(e.get("ts")) or _parse_ts(data.get("ts"))
        fires_by_session.setdefault(sid, []).append((dt or datetime.max, data.get("ci")))

    by_id = {n.get("id"): n for n in graph_nodes if n.get("id")}
    fixes: dict[str, list[dict]] = {}
    for n in graph_nodes:
        origin = n.get("caused_by")
        if origin:
            fixes.setdefault(origin, []).append(n)
    # Same w4 gate as _survival: without any causal telemetry, _node_outcome's
    # "no fix found" branch is indistinguishable from "revert data doesn't exist
    # yet", so a shipped row lands in `shipped_untracked`, never a fake merged_clean.
    w4_available = any(("reverted" in n) or n.get("caused_by") for n in graph_nodes)

    joined_rows = 0
    transcript_rows = 0
    node_linked = 0
    ci_unparsed_total = 0
    outcome_tracked = 0
    shipped_rows = 0

    # Plan-thread cost vs build-thread cost per node (the node's one extra line):
    # sum `planned` spend and shipped spend per graph_node_id, emitted only for
    # nodes that carry at least one planned row.
    plan_cost_by_node: dict[str, float] = {}
    build_cost_by_node: dict[str, float] = {}

    buckets: dict[str, dict] = {}
    per_row_fires: list[int | None] = []
    per_row_reds: list[int | None] = []
    per_row_tokens: list[float | None] = []
    per_row_duration: list[float | None] = []

    for r in windowed:
        sids = _row_session_ids(r)
        fires = []
        for sid in sids:
            fires.extend(fires_by_session.get(sid, []))
        fires.sort(key=lambda pair: pair[0])
        ci_ordered = [ci for _, ci in fires]

        loop_fires = len(fires) if fires else None  # None (not 0): no joined event != measured zero
        if loop_fires:
            joined_rows += 1
            ci_reds, unparsed = _ci_reds_from_fires(ci_ordered)
            ci_unparsed_total += unparsed
        else:
            ci_reds = None

        turns, toolcalls = _transcript_counts(sids, read_transcript)
        if turns is not None:
            transcript_rows += 1

        nid = r.get("graph_node_id")
        if nid:
            node_linked += 1

        shipped = _is_shipped_reason(r.get("termination_reason"))
        if shipped:
            shipped_rows += 1
            if w4_available and nid and nid in by_id:
                cls = _node_outcome(nid, _parse_ts(r.get("completed")), by_id, fixes)
                outcome_tracked += 1
            else:
                cls = "shipped_untracked"
            if nid:
                build_cost_by_node[nid] = build_cost_by_node.get(nid, 0.0) + _num(r.get("cost_usd"))
        elif _is_planned_row(r):
            cls = "planned"
            if nid:
                plan_cost_by_node[nid] = plan_cost_by_node.get(nid, 0.0) + _num(r.get("cost_usd"))
        elif (r.get("termination_reason") or "") == "delegated":
            # A handed-off thread is not waste; keep it out of `unshipped`.
            cls = "delegated"
        else:
            cls = "unshipped"

        tokens = _num_opt(r.get("tokens_total"))  # None (not 0) when unmeasurable
        duration = _num_opt(r.get("duration_minutes"))
        b = buckets.setdefault(cls, {"n": 0, "spend_usd": 0.0, "tokens": [], "fires": [], "duration": []})
        b["n"] += 1
        b["spend_usd"] += _num(r.get("cost_usd"))  # spend is a SUM: a missing cost is 0, not None
        if tokens is not None:
            b["tokens"].append(tokens)
        if duration is not None:
            b["duration"].append(duration)
        if loop_fires is not None:
            b["fires"].append(loop_fires)

        # Distribution population = rows with >=1 joined fire (a session that
        # never emitted loop_check would otherwise drag every median down).
        if loop_fires is not None:
            per_row_fires.append(loop_fires)
            per_row_reds.append(ci_reds)
            per_row_tokens.append(tokens)
            per_row_duration.append(duration)

    per_class = {
        cls: {
            "n": b["n"],
            "spend_usd": round(b["spend_usd"], 2),
            "median_tokens": _percentile(b["tokens"], 50),
            "median_fires": _percentile(b["fires"], 50),
            "median_duration_min": _percentile(b["duration"], 50),
        }
        for cls, b in buckets.items()
    }

    return {
        "state": "ok",
        "since_days": since_days,
        "coverage": {
            "rows": total,
            "loop_join_pct": _pct(joined_rows, total),
            "transcript_pct": _pct(transcript_rows, total),
            "node_linkage_pct": _pct(node_linked, total),
            "outcome_tracked_pct": _pct(outcome_tracked, shipped_rows),
            "ci_unparsed": ci_unparsed_total,
        },
        "per_outcome_class": per_class,
        "plan_vs_build_cost": {
            nid: {
                "plan_usd": round(plan_cost_by_node[nid], 2),
                "build_usd": round(build_cost_by_node.get(nid, 0.0), 2),
            }
            for nid in plan_cost_by_node
        },
        "distribution": {
            "loop_fires": _dist(per_row_fires),
            "ci_reds": _dist(per_row_reds),
            "tokens_total": _dist(per_row_tokens),
            "duration_minutes": _dist(per_row_duration),
        },
    }


# ── plan fidelity (x-ed6b3294) ───────────────────────────────────────────────
# Grades PLANNING quality, not build quality: join each planning thread's plan
# doc to its delivery (PR diff + SUMMARY.md) and score how well the plan held up.
# The score is attributed to the PLANNING session_id.

_AC_ID_RE = re.compile(r"AC\d+-[A-Z]+")
_OWNERSHIP_FILE_RE = re.compile(r"`([^`]+/[^`]+|[^`]+\.\w+)`")
_DATA_MODEL_RE = re.compile(r"(migration|schema|(?:^|/)models?\.py|\.sql|models?/|migrations?/)", re.IGNORECASE)


def _plan_key(plan_path, project=None) -> str | None:
    """Normalize a plan_path for join, scoped to a `project`. Uses the last two
    path segments (parent dir + file) rather than the bare basename, so a
    recurring folder-plan filename (`00-INDEX.md`) does not collapse unrelated
    plans in the GLOBAL ledger; the `project` prefix scopes across repos. Still
    prefix-independent, so the planning row and its shipped row match regardless
    of the worktree each was stamped under."""
    if not isinstance(plan_path, str) or not plan_path:
        return None
    p = Path(plan_path.split("#", 1)[0])
    tail = f"{p.parent.name}/{p.name}" if p.parent.name else p.name
    if not tail or tail == "/":
        return None
    return f"{project or ''}::{tail}"


def _parse_ac_ids(doc: str) -> set[str]:
    return set(_AC_ID_RE.findall(doc))


def _parse_ownership_files(doc: str) -> set[str]:
    """File paths listed under the plan's `File Ownership Map` section."""
    idx = doc.find("File Ownership Map")
    section = doc[idx:] if idx != -1 else ""
    files: set[str] = set()
    for line in section.splitlines():
        if line.lstrip().startswith("|") and ("modify" in line or "create" in line or "delete" in line):
            files.update(_OWNERSHIP_FILE_RE.findall(line))
    return files


def _is_data_model_file(path: str) -> bool:
    return bool(_DATA_MODEL_RE.search(path))


def _path_match(a: str, b: str) -> bool:
    """Two repo-relative paths refer to the same file: equal, or one is the
    other's tail on a path-component boundary. The boundary guard stops
    `some_other_fold.py` from matching `fold.py`."""
    return a == b or a.endswith("/" + b) or b.endswith("/" + a)


def _score_fidelity(plan_doc: str | None, summary: str | None, diff_files: list[str] | None) -> dict:
    """The three AC-required metrics + deviation load. Each degrades to n/a
    (None) when its input is missing - an unmeasurable plan is never a 0% plan."""
    # (a) AC coverage: planned ACs mentioned in SUMMARY / total planned ACs.
    ac_cov: dict | None = None
    if plan_doc is not None and summary is not None:
        acs = _parse_ac_ids(plan_doc)
        if acs:
            verified = sum(1 for ac in acs if ac in summary)
            ac_cov = {"verified": verified, "total": len(acs), "pct": _pct(verified, len(acs))}

    # (b) scope drift: diff files absent from the ownership map (unplanned) +
    # planned files never touched (untouched).
    scope: dict | None = None
    # (c) data-model surprise: data-model files in the diff the plan did not list.
    dm_surprise: int | None = None
    if plan_doc is not None and diff_files is not None:
        owned = _parse_ownership_files(plan_doc)
        unplanned = [f for f in diff_files if not any(_path_match(f, o) for o in owned)]
        untouched = [o for o in owned if not any(_path_match(f, o) for f in diff_files)]
        scope = {"unplanned": sorted(unplanned), "untouched": sorted(untouched)}
        dm_surprise = sum(1 for f in unplanned if _is_data_model_file(f))

    # (d) deviation load: reasoned deviations are signal, not a penalty - counted, not scored.
    deviations: int | None = None
    if summary is not None:
        deviations = summary.lower().count("deviation") + summary.count("<help") + summary.count("STOP")

    return {
        "ac_coverage": ac_cov,
        "scope_drift": scope,
        "data_model_surprise": dm_surprise,
        "deviation_load": deviations,
    }


def _default_read_plan_doc(plan_path: str) -> str | None:
    p = Path(plan_path.split("#", 1)[0])
    try:
        return p.read_text(encoding="utf-8") if p.is_file() else None
    except OSError:
        return None


def _declared_probes(plan_doc: str | None) -> list[str]:
    """`done_probes` declared in a plan doc's frontmatter (x-e54c)."""
    if not plan_doc or not plan_doc.lstrip().startswith("---"):
        return []
    body = plan_doc.lstrip()[3:].split("\n---", 1)
    if len(body) < 2:
        return []
    try:
        import yaml

        fm = yaml.safe_load(body[0])
    except Exception:
        return []
    probes = fm.get("done_probes") if isinstance(fm, dict) else None
    return [str(p) for p in probes] if isinstance(probes, list) else []


def _probe_evidence(loop_check_events: list[dict], session_id: str | None) -> dict:
    """The delivery session's LAST recorded probe results, keyed command->state.

    Last wins: a session may block on a failing probe and pass on a later fire,
    and only the fire that granted done is evidence.
    """
    if not session_id:
        return {}
    latest: dict = {}
    for e in loop_check_events:
        data = e.get("data") or {}
        if data.get("session_id") != session_id:
            continue
        probes = data.get("done_probes")
        if isinstance(probes, dict) and probes:
            latest = probes
    return latest


def _default_read_summary(row: dict) -> str | None:
    sp = row.get("summary_path")
    if isinstance(sp, str) and sp:
        try:
            p = Path(sp)
            if p.is_file():
                return p.read_text(encoding="utf-8")
        except OSError:
            pass
    s = row.get("summary")
    return s if isinstance(s, str) else None


def _default_read_diff(row: dict) -> list[str] | None:
    pr_number = row.get("pr_number")
    if not pr_number:
        return None
    import subprocess

    # The ledger is global (cross-repo), so a bare `gh pr diff <n>` resolves the
    # number in the operator's cwd and can diff an unrelated PR. Pin the repo
    # from the delivery row's pr_url so the diff always targets the right repo.
    cmd = ["gh", "pr", "diff", str(pr_number), "--name-only"]
    pr_url = row.get("pr_url")
    if isinstance(pr_url, str):
        m = re.search(r"github\.com/([^/]+/[^/]+?)(?:\.git)?/pull/", pr_url)
        if m:
            cmd += ["--repo", m.group(1)]
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.SubprocessError):
        return None
    if out.returncode != 0:
        return None
    return [ln.strip() for ln in out.stdout.splitlines() if ln.strip()]


def build_plan_fidelity(
    rows: list[dict],
    graph_nodes: list[dict],
    *,
    since_days: int,
    now: datetime,
    read_plan_doc=None,
    read_summary=None,
    read_diff=None,
    loop_check_events: list[dict] | None = None,
) -> dict:
    """Join each `planned` row (W1) to its delivery and score plan fidelity.

    A planned row with no joinable delivery is `unjoined`, never scored 0% -
    an unimplemented plan is unmeasurable, not a bad plan (the fold's
    coverage-honesty rule)."""
    read_plan_doc = read_plan_doc or _default_read_plan_doc
    read_summary = read_summary or _default_read_summary
    read_diff = read_diff or _default_read_diff
    loop_check_events = loop_check_events or []

    cutoff = now - timedelta(days=since_days)

    def _in_window(ts_raw) -> bool:
        dt = _parse_ts(ts_raw)
        return dt is not None and cutoff <= dt <= now

    windowed = [r for r in rows if _in_window(r.get("completed"))]
    if not windowed:
        return {"state": "no_data", "since_days": since_days, "rows": 0}

    by_id = {n.get("id"): n for n in graph_nodes if n.get("id")}
    fixes: dict[str, list[dict]] = {}
    for n in graph_nodes:
        origin = n.get("caused_by")
        if origin:
            fixes.setdefault(origin, []).append(n)

    shipped_by_plan: dict[str, list[dict]] = {}
    for r in windowed:
        if _is_shipped_reason(r.get("termination_reason")):
            key = _plan_key(r.get("plan_path"), r.get("project"))
            if key:
                shipped_by_plan.setdefault(key, []).append(r)

    results: list[dict] = []
    joined = 0
    for r in windowed:
        if not _is_planned_row(r):
            continue
        plan_path = r.get("plan_path")
        key = _plan_key(plan_path, r.get("project"))
        deliveries = shipped_by_plan.get(key, []) if key else []
        sid = r.get("session_id")
        if not deliveries:
            results.append({"session_id": sid, "plan_path": plan_path, "status": "unjoined"})
            continue
        joined += 1
        d = deliveries[0]
        nid = d.get("graph_node_id")
        plan_doc = read_plan_doc(plan_path) if plan_path else None
        score = _score_fidelity(plan_doc, read_summary(d), read_diff(d))
        # x-e54c: join "the plan said this would prove it" to "evidence it ran".
        # A plan declaring no probes reports null, never a fabricated 0/0.
        declared = _declared_probes(plan_doc)
        probes = None
        if declared:
            evidence = _probe_evidence(loop_check_events, d.get("session_id"))
            probes = {
                "declared": len(declared),
                "passed": sum(1 for c in declared if evidence.get(c) == "pass"),
            }
        results.append({
            "session_id": sid,
            "plan_path": plan_path,
            "status": "joined",
            "pr_number": d.get("pr_number"),
            "outcome": _node_outcome(nid, _parse_ts(d.get("completed")), by_id, fixes) if nid and nid in by_id else None,
            "probes": probes,
            **score,
        })

    planned_total = len(results)
    return {
        "state": "ok",
        "since_days": since_days,
        "results": results,
        "coverage": {"planned_rows": planned_total, "joined_pct": _pct(joined, planned_total)},
    }


if __name__ == "__main__":
    # ponytail self-check: the fold's load-bearing invariants, no framework.
    now = datetime(2026, 7, 3, 20, 0, 0)
    rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1", "cost_usd": 5.0},
        {"completed": "2026-07-02T10:00:00", "termination_reason": "NoProgress", "graph_node_id": "x-2", "cost_usd": 2.0},
        {"completed": "2026-07-01T10:00:00", "cost_usd": 1.0},  # no termination_reason
        {"completed": "2020-01-01T00:00:00", "termination_reason": "DonePRGreen", "cost_usd": 99.0},  # out of window
    ]
    sb = build_scoreboard(rows, [], [], since_days=28, now=now)
    assert sb["state"] == "partial", sb
    assert sb["coverage"]["rows"] == 3, sb  # 4th row excluded by window
    assert sb["coverage"]["termination_reason_pct"] == 67, sb  # 2 of 3
    assert sb["stop_cause"] == {"DonePRGreen": 1, "NoProgress": 1}, sb
    assert sb["spend"] == {"ship_terminal_usd": 5.0, "wedge_terminal_usd": 2.0, "other_usd": 1.0}, sb
    assert sb["autonomy"]["available"] is False, sb
    assert sb["survival"]["available"] is False, sb

    # no-data: empty row set (fresh install / nothing in window)
    assert build_scoreboard([], [], [], since_days=28, now=now)["state"] == "no_data"

    # survival compute activates when a node carries a W4 causal field
    g = [
        {"id": "x-1", "reverted": False},
        {"id": "x-9", "caused_by": "x-2", "created_at": "2026-07-03T00:00:00"},
    ]
    ship_only = [{"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1", "cost_usd": 5.0}]
    sb2 = build_scoreboard(ship_only, [{"type": "human_touch", "ts": "2026-07-03T09:00:00"}], g, since_days=28, now=now)
    assert sb2["survival"]["available"] is True and sb2["survival"]["survived"] == 1, sb2
    assert sb2["autonomy"]["available"] is True and sb2["autonomy"]["touches"] == 1, sb2

    # equivalent instants in different offsets land on the same local timeline
    # (tz-agnostic: no absolute-value assertion, so CI-UTC == laptop-PDT)
    assert _parse_ts("2026-07-03T12:00:00+02:00") == _parse_ts("2026-07-03T10:00:00Z")
    # a malformed cost never crashes the fold
    assert _num("junk") == 0.0 and _num(None) == 0.0 and _num("5.5") == 5.5
    # {"entries": null} is valid JSON, junk shape -> empty, never a crash
    import tempfile as _tf, os as _os
    _fd, _p = _tf.mkstemp()
    _os.write(_fd, b'{"entries": null}'); _os.close(_fd)
    assert load_ledger_rows(Path(_p)) == []
    _os.unlink(_p)

    # --by-skill: transcript-scan wins over phase-proxy; neither -> unattributed
    def _fake_read(sid):
        return [json.dumps({"message": {"content": [{"type": "tool_use", "name": "Skill", "input": {"skill": "fno:review"}}]}})]

    skill_rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1", "cost_usd": 4.0,
         "sessions": ["11111111-1111-1111-1111-111111111111"]},
        {"completed": "2026-07-02T10:00:00", "termination_reason": "NoProgress", "phases_completed": ["do"], "cost_usd": 1.0},
        {"completed": "2026-07-01T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-9", "cost_usd": 2.0},
    ]
    sb3 = build_skill_scoreboard(
        skill_rows, [{"id": "x-1", "reverted": False}], [],
        since_days=28, now=now, read_transcript=_fake_read, resolve_skill_version=lambda s, ts: "abc123",
    )
    by_skill = {r["skill"]: r for r in sb3["rows"]}
    assert by_skill["fno:review"]["method"] == "transcript" and by_skill["fno:review"]["runs"] == 1, sb3
    assert by_skill["fno:do"]["method"] == "phase-proxy" and by_skill["fno:do"]["runs"] == 1, sb3
    assert by_skill["unattributed"]["runs"] == 1, sb3
    assert sb3["coverage"]["attributed_pct"] == 67, sb3  # 2 of 3 rows attributed

    # --efficiency AC2: a red streak counts as ONE episode, not per-poll.
    assert _ci_reds_from_fires(["SUCCESS", "FAILURE:smoke", "FAILURE:smoke", "SUCCESS"]) == (1, 0)
    assert _ci_reds_from_fires(["FAILURE:x", "SUCCESS", "FAILURE:y"]) == (2, 0)  # two distinct episodes
    # AC6: an unrecognized ci shape -> ci_reds None + counted unparsed.
    assert _ci_reds_from_fires(["SUCCESS", "WEIRD_SHAPE"]) == (None, 1)
    # unknown/skipped are recognized-benign (plan deviation), not drift.
    assert _ci_reds_from_fires(["unknown", "skipped", "SUCCESS"]) == (0, 0)

    eff_rows = [
        {"completed": "2026-07-03T10:00:00", "termination_reason": "DonePRGreen", "graph_node_id": "x-1",
         "cost_usd": 5.0, "tokens_total": 1000, "duration_minutes": 30, "sessions": ["s-a"]},
        {"completed": "2026-07-02T10:00:00", "termination_reason": "NoProgress", "graph_node_id": "x-2",
         "cost_usd": 2.0, "tokens_total": 500, "duration_minutes": 10, "sessions": ["s-none"]},
    ]
    eff_events = [
        {"ts": "2026-07-03T09:00:00Z", "type": "loop_check", "data": {"session_id": "s-a", "ci": "FAILURE:smoke"}},
        {"ts": "2026-07-03T09:05:00Z", "type": "loop_check", "data": {"session_id": "s-a", "ci": "SUCCESS"}},
    ]
    eff = build_efficiency(
        eff_rows, eff_events, [{"id": "x-1", "reverted": False}],
        since_days=28, now=now, read_transcript=lambda sid: None,
    )
    assert eff["state"] == "ok", eff
    assert eff["coverage"]["rows"] == 2, eff
    assert eff["coverage"]["loop_join_pct"] == 50, eff  # only s-a joined; s-none row missed
    assert eff["per_outcome_class"]["merged_clean"]["n"] == 1, eff
    assert eff["per_outcome_class"]["unshipped"]["n"] == 1, eff
    assert eff["distribution"]["loop_fires"]["n"] == 1, eff  # the no-event row is excluded
    assert eff["distribution"]["ci_reds"]["median"] == 1, eff
    # AC5: a windowed row whose sessions match no events -> None, not 0.
    solo = build_efficiency(
        [eff_rows[1]], [], [], since_days=28, now=now, read_transcript=lambda sid: None,
    )
    assert solo["coverage"]["loop_join_pct"] == 0, solo
    assert solo["distribution"]["loop_fires"] == {"median": None, "p90": None, "n": 0}, solo
    # no-data window
    assert build_efficiency([], [], [], since_days=28, now=now)["state"] == "no_data"

    print("fold self-check OK")
