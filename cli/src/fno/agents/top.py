"""``fno agents top`` (x-c5cc): every live worker process with RSS.

One table over the SAME union the spawn gate counts (imported from
``spawn_gate.census`` - never duplicated), so the debugging surface and the
enforcement surface can never disagree. Python-only by design (LD8): RSS via
psutil, no daemon involvement, kept out of the Rust client verb list.

With ``--subagents`` (x-af92), appends a read-only section listing
harness-native subagents (sidechain 'limbs') that the pid/registry census
cannot see. That section is display-only: it never feeds the spawn gate's
slot count, and a subagent has no mail handle, so it is observable but not
addressable. See docs/architecture/coordination.md.
"""
from __future__ import annotations

import json
from typing import Optional

from fno.agents.discover import (
    _SUBAGENT_SCAN_WINDOW_S,
    _subagent_live_seconds,
    discover_subagents,
)
from fno.agents.spawn_gate import LiveWorker, census


def _rss_mb(pid: Optional[int]) -> Optional[int]:
    if not pid:
        return None
    try:
        import psutil

        return int(psutil.Process(pid).memory_info().rss / (1024 * 1024))
    except Exception:
        return None


def _crown_map() -> dict[str, str]:
    """name -> crown_label for crowned registry rows (US9). Best-effort: a read
    failure degrades to no crowns rather than breaking the process view."""
    try:
        from fno.agents.registry import load_registry

        return {e.name: e.crown_label for e in load_registry() if e.crown_label}
    except Exception:  # noqa: BLE001 — top is a debug view, never fail on it
        return {}


def _registry_handles() -> dict[str, str]:
    """session uuid -> registry handle. Best-effort, like :func:`_crown_map`.

    `top` labels a foreign claude row with the FIRST 8 hex of the session uuid;
    the registry handle for that same session is the LAST 8. Same session, two
    identities, and nothing on screen relates them -- which is how a grep across
    the two views returned nothing and got read as "all agents are dead".
    """
    try:
        from fno.agents.registry import load_registry

        return {
            e.harness_session_id: e.name
            for e in load_registry()
            if e.harness_session_id and e.name
        }
    except Exception:  # noqa: BLE001 — top is a debug view, never fail on it
        return {}


def _progress_map(workers: list[LiveWorker]) -> dict[str, Optional[str]]:
    """name -> progress verdict, for the fno-registry-sourced rows only.

    This is the surface that showed 8513 MB across 31 live pids with no way
    to see which of them were parked (specimen 1), so it must show progress
    beside RSS on the same line. Foreign claude rows (``source == "claude"``,
    no fno registry entry) are out of scope: there is no ``harness`` /
    ``route_settings_path`` context here to judge a refusal against.

    One transcript read per row (``resolve_session_truth``), reused for both
    the reachability verdict and the progress verdict -- the same shape
    ``fno.agents.read`` uses so this view does not pay a second read for the
    same evidence.
    """
    from fno.agents.reachability import classify_progress, classify_reachability, registry_falsifier
    from fno.agents.registry import load_registry
    from fno.agents.session_truth import resolve_session_truth

    try:
        entries = {e.name: e for e in load_registry()}
    except Exception:  # noqa: BLE001 — top is a debug view, never fail on it
        return {}

    out: dict[str, Optional[str]] = {}
    for w in workers:
        if w.source != "fno":
            continue
        entry = entries.get(w.name)
        if entry is None:
            continue
        truth = resolve_session_truth(w.name)
        truth_state = truth.get("state")
        reach = classify_reachability(
            truth_state=truth_state,
            age_s=truth.get("last_activity_age_s"),
            falsifier=registry_falsifier(entry),
        )
        prog = classify_progress(
            truth_state=truth_state,
            reachability=reach.verdict,
            observed_model=truth.get("observed_model"),
            harness=w.harness,
            route_settings_path=entry.route_settings_path,
        )
        out[w.name] = prog.verdict
    return out


def _rows(workers: list[LiveWorker], crowns: dict[str, str]) -> list[dict]:
    handles = _registry_handles()
    progress = _progress_map(workers)
    rows = []
    for w in workers:
        # Null when this session has no registry row (a foreign claude session
        # that fno never adopted), which is a real answer, not a lookup miss.
        handle = handles.get(w.session_id or "")
        rows.append(
            {
                "source": w.source,
                "name": w.name,
                "handle": handle if handle != w.name else None,
                # HARNESS, not PROVIDER: the value is `row.harness` (the CLI),
                # so the old name made a claude-hosted worker on a z.ai route
                # read as running on claude. Same rename as the list row.
                "harness": w.harness,
                "substrate": w.substrate,
                "pid": w.pid,
                "rss_mb": _rss_mb(w.pid),
                "status": w.status,
                # The orthogonal axis beside `status`: null for a foreign
                # claude row this view has no harness/route context to judge.
                "progress": progress.get(w.name),
                "crown": crowns.get(w.name),  # US9: null when uncrowned
            }
        )
    # Heaviest first: the row the operator is looking for when RAM is tight.
    rows.sort(key=lambda r: -float(r["rss_mb"] or 0))
    return rows


def _fmt_age(seconds: float) -> str:
    """Compact floored age: 45s / 12m / 3h."""
    s = int(seconds)
    if s < 60:
        return f"{s}s"
    if s < 3600:
        return f"{s // 60}m"
    return f"{s // 3600}h"


def _subagent_section() -> dict:
    """Read-only sidechain rows for the --subagents section (x-af92).

    Returns the rendered rows, any scan warning, and the live threshold so the
    caller can state the threshold in its header. Carrying the threshold out
    keeps the verdict definition next to the verdict rather than re-derived.
    """
    found, warnings = discover_subagents()
    rows = [
        {
            "agent_id": s.agent_id,
            "parent": s.parent_session_id[:8],
            "branch": s.git_branch or "",
            "age": _fmt_age(s.age_seconds),
            "verdict": s.verdict,
            "cwd": s.cwd or "",
        }
        for s in found
    ]
    return {
        "rows": rows,
        "warnings": warnings,
        "live_threshold": int(_subagent_live_seconds()),
        "scan_window_h": int(_SUBAGENT_SCAN_WINDOW_S // 3600),
    }


def _render_subagent_lines(section: dict) -> list[str]:
    """The human-readable sidechain block, scope-stated even when empty."""
    rows = section["rows"]
    threshold = section["live_threshold"]
    window_h = section["scan_window_h"]
    out = [
        f"subagents (claude only; active = mtime within {threshold}s; "
        f"older rows age out after {window_h}h)"
    ]
    out.append(
        f"{'AGENT':<16} {'PARENT':<9} {'BRANCH':<12} {'AGE':>5} {'VERDICT':<8} CWD"
    )
    if not rows:
        # AC7-EDGE: report the claude-only scope, not an empty list that reads
        # as "none running" - a non-claude host has no measured layout here.
        out.append(
            "none in the scan window (claude only; "
            "codex/opencode/agy task layouts not measured)"
        )
        return out
    for r in rows:
        out.append(
            f"{r['agent_id']:<16} {r['parent']:<9} {r['branch'] or '-':<12} "
            f"{r['age']:>5} {r['verdict']:<8} {r['cwd'] or '-'}"
        )
    return out


def render_top(as_json: bool = False, include_subagents: bool = False) -> str:
    """Render the union table (or its JSON mirror - same rows, LD: parity).

    ``include_subagents`` appends a read-only sidechain section (x-af92); in
    JSON it adds a ``subagents`` key and folds the scan warnings in.
    """
    c = census()
    rows = _rows(c.workers, _crown_map())
    subagents = _subagent_section() if include_subagents else None
    if as_json:
        payload: dict = {
            "workers": rows,
            "slot_claims": c.slot_claims,
            "warnings": list(c.warnings),
        }
        if subagents is not None:
            payload["subagents"] = subagents["rows"]
            payload["warnings"] = c.warnings + subagents["warnings"]
        return json.dumps(payload, indent=2)

    out: list[str] = []
    out.extend(c.warnings)
    header = (
        f"{'SOURCE':<7} {'NAME':<24} {'HARNESS':<9} {'SUBSTRATE':<10} "
        f"{'PID':>7} {'RSS_MB':>7} {'PROGRESS':<17} STATUS"
    )
    out.append(header)
    if not rows:
        out.append("no live workers")
    for r in rows:
        # US9: mark a crowned worker in the name cell (ASCII, alignment-safe).
        # The registry handle rides along when it differs from this view's own
        # label, so `top` and `list` can be joined by eye instead of by guessing
        # which end of the uuid each one truncated.
        name_cell = r["name"] + (f" [{r['crown']}]" if r["crown"] else "")
        if r.get("handle"):
            name_cell += f" ={r['handle']}"
        out.append(
            f"{r['source']:<7} {name_cell:<24} {r['harness']:<9} "
            f"{r['substrate']:<10} {r['pid'] or '-':>7} "
            f"{r['rss_mb'] if r['rss_mb'] is not None else '-':>7} "
            f"{r['progress'] or '-':<17} {r['status']}"
        )
    if c.slot_claims:
        out.append(f"(+{c.slot_claims} queued headless slot claim(s))")
    if subagents is not None:
        out.append("")
        out.extend(subagents["warnings"])
        out.extend(_render_subagent_lines(subagents))
    return "\n".join(out)
