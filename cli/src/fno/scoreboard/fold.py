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
import time
from collections import Counter
from datetime import datetime, timedelta
from pathlib import Path

# termination_reason -> outcome class. "Done*" is a delivered ship; the wedge
# set is the stuck-terminal set. Everything else (Interrupted, delegated,
# NoWork, or no reason at all) is neither and lands in "other" so the spend
# split always reconciles to the window total.
_WEDGE_REASONS = frozenset({"NoProgress", "Budget", "Aborted"})

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
        if tr and tr.startswith("Done"):
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
    ship_rows = [r for r in windowed if (r.get("termination_reason") or "").startswith("Done")]
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
        if nid and (r.get("termination_reason") or "").startswith("Done"):
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
        content = (obj.get("message") or {}).get("content")
        if not isinstance(content, list):
            continue
        for block in content:
            if isinstance(block, dict) and block.get("type") == "tool_use" and block.get("name") == "Skill":
                skill = (block.get("input") or {}).get("skill")
                if skill:
                    found.append(skill)
    return found


def _default_read_transcript(session_id: str) -> list[str] | None:
    from fno.cost._session_cost import find_transcript

    path = find_transcript(session_id)
    if not path:
        return None
    try:
        return Path(path).read_text(encoding="utf-8").splitlines()
    except OSError:
        return None


def _default_skill_version(skill_id: str, ts_raw: str | None) -> str:
    """Best-effort git hash of the skill's SKILL.md at (or before) ts_raw.
    No git history / no such file / any subprocess failure -> "unknown",
    never an error (AC-EDGE)."""
    import subprocess

    from fno.paths import resolve_repo_root

    name = skill_id.split(":", 1)[1] if ":" in skill_id else skill_id
    rel = f"skills/{name}/SKILL.md"
    try:
        root = resolve_repo_root()
    except Exception:
        return "unknown"
    if not (root / rel).exists():
        return "unknown"
    until = ts_raw or datetime.now().isoformat()
    try:
        out = subprocess.run(
            ["git", "log", "-1", "--format=%h", f"--until={until}", "--", rel],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return "unknown"
    return out.stdout.strip() or "unknown"


def _extract_skill_runs(row: dict, *, read_transcript) -> tuple[list[str], str]:
    """Return (distinct skill ids, method) for one ledger row.

    method is "transcript" (real Skill tool_use found), "phase-proxy"
    (fallback to phases_completed), or "unattributed" (neither)."""
    from fno.cost._session_cost import UUID_RE

    uuid_sessions = [s for s in (row.get("sessions") or []) if isinstance(s, str) and UUID_RE.match(s)]
    found: list[str] = []
    for sid in uuid_sessions:
        lines = read_transcript(sid)
        if lines:
            found.extend(_skills_from_transcript(lines))
    if found:
        return sorted(set(found)), "transcript"

    proxy = sorted({_PHASE_TO_SKILL[p] for p in (row.get("phases_completed") or []) if p in _PHASE_TO_SKILL})
    if proxy:
        return proxy, "phase-proxy"
    return [], "unattributed"


def _new_skill_bucket() -> dict:
    return {"runs": 0, "shipped": 0, "reverted": 0, "cost_total": 0.0, "touches_total": 0, "method": "unattributed"}


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

    touches_by_node: Counter = Counter()
    for e in touch_events:
        nid = e.get("graph_node_id") or (e.get("data") or {}).get("graph_node_id")
        if nid:
            touches_by_node[nid] += 1

    buckets: dict[tuple[str, str], dict] = {}
    attributed = 0

    for r in windowed:
        skills, method = _extract_skill_runs(r, read_transcript=read_transcript)
        if not skills:
            b = buckets.setdefault(("unattributed", "-"), _new_skill_bucket())
            b["runs"] += 1
            continue
        attributed += 1

        shipped = (r.get("termination_reason") or "").startswith("Done")
        nid = r.get("graph_node_id")
        cost = _num(r.get("cost_usd"))
        touches = touches_by_node.get(nid, 0) if nid else 0
        outcome = _node_outcome(nid, _parse_ts(r.get("completed")), by_id, fixes) if (shipped and nid) else None

        for skill in skills:
            version = resolve_skill_version(skill, r.get("completed"))
            b = buckets.setdefault((skill, version), _new_skill_bucket())
            b["runs"] += 1
            b["cost_total"] += cost
            b["touches_total"] += touches
            b["method"] = method
            if shipped:
                b["shipped"] += 1
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
                "revert_rate_pct": _pct(b["reverted"], b["shipped"]),
                "touches_per_run": round(b["touches_total"] / runs, 2) if runs else 0.0,
                "cost_per_run": round(b["cost_total"] / runs, 2) if runs else 0.0,
                "method": b["method"],
            }
        )
    out_rows.sort(key=lambda x: (-x["runs"], x["skill"]))

    return {
        "state": "ok",
        "since_days": since_days,
        "coverage": {"rows": total, "attributed_pct": _pct(attributed, total)},
        "rows": out_rows,
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

    print("fold self-check OK")
