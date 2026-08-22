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
from pathlib import Path
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
    """name -> crown label for crowned registry rows (US9), sourced from
    :func:`crown_reading` so this view and ``fno whoami`` cannot drift into two
    renderings of the same fact. Best-effort: a read failure degrades to no
    crowns rather than breaking the process view."""
    try:
        from fno.agents.crown import crown_reading
        from fno.agents.registry import load_registry

        out: dict[str, str] = {}
        for e in load_registry():
            reading = crown_reading(e)
            if reading is not None:
                out[e.name] = reading["label"]
        return out
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
            last_activity_age_s=truth.get("last_activity_age_s"),
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
        # The registry keys a foreign claude row by the LAST 8 hex of the
        # session uuid and this view labels it by the FIRST 8, so the crown
        # must join through the session id the handle column already
        # resolves - not through the display name, which reads None for
        # exactly the row `handle` above exists to bridge.
        reg_name = handle or w.name
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
                "crown": crowns.get(reg_name),  # US9: null when uncrowned
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


# How much of the global journal `pane_counter_rows` reads: enough tail to
# hold several cadence snapshots of a busy day (the mux emits every 30s, so
# two samples sit within ~1 KB of each other) without slurping a multi-MB log.
_PANE_COUNTERS_TAIL_BYTES = 512 * 1024

# The five monotonic totals the mux emits per pane. Differenced per field.
_PANE_COUNTER_FIELDS = (
    "bytes_in",
    "grid_updates",
    "frames_composited",
    "frames_emitted",
    "cpu_ns",
)


def pane_counter_rows(events_path: Optional[Path] = None) -> dict:
    """Difference the last two ``mux_pane_counters`` snapshots in the journal.

    THE one reader for per-pane mux counters: ``fno agents top --pane-stats``
    renders it here, and the spawn gate's pane-vs-bg-session pricing imports
    this same function rather than growing a second implementation. The mux
    emits monotonic TOTALS, never rates - the delta over the window between
    two samples is computed here, per pane.

    Returns ``{status, rows, born, gone, session, window_s}``. ``status`` is
    ``ok`` | ``insufficient-samples`` | ``unreadable``; an honest message is
    always renderable because a missing second sample or a broken journal must
    never read as "no cost" (the empty-table trap). Panes are matched across
    samples only when BOTH carry the same mux session name - a server restart
    resets pane ids and totals, so a session change reports every pane as
    born/gone instead of differencing across the reset.
    """
    from fno.paths import global_events_json

    path = events_path if events_path is not None else global_events_json()
    empty = {"status": "insufficient-samples", "rows": [], "born": [], "gone": [], "session": None, "window_s": None}
    try:
        size = path.stat().st_size
        with path.open("rb") as fh:
            if size > _PANE_COUNTERS_TAIL_BYTES:
                fh.seek(size - _PANE_COUNTERS_TAIL_BYTES)
                fh.readline()  # drop the partial line the seek landed in
            tail = fh.read().decode("utf-8", errors="replace")
        samples = []
        for line in tail.splitlines():
            if '"mux_pane_counters"' not in line:
                continue  # cheap pre-filter: the journal carries many types
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if ev.get("type") == "mux_pane_counters":
                samples.append(ev)
    except FileNotFoundError:
        return empty  # no journal at all = no samples, not a broken read
    except OSError as exc:
        return {**empty, "status": "unreadable", "error": f"{type(exc).__name__}: {exc}"}

    if len(samples) < 2:
        return empty

    older, newer = samples[-2], samples[-1]
    older_panes = {p["pane_id"]: p for p in older.get("data", {}).get("panes", [])}
    newer_panes = {p["pane_id"]: p for p in newer.get("data", {}).get("panes", [])}
    session = newer.get("data", {}).get("session")
    restart = older.get("data", {}).get("session") != session

    def _ts_seconds(ev: dict) -> Optional[float]:
        try:
            from datetime import datetime

            return datetime.fromisoformat(ev["ts"].replace("Z", "+00:00")).timestamp()
        except Exception:
            return None

    t0, t1 = _ts_seconds(older), _ts_seconds(newer)
    window_s = round(t1 - t0, 1) if t0 is not None and t1 is not None else None

    rows = []
    for pid, p in sorted(newer_panes.items()):
        q = older_panes.get(pid)
        if q is None or restart:
            # Only in the newer sample (or ids reset across a restart): a
            # birth, not a delta - one sample cannot be differenced.
            continue
        row = {"pane_id": pid, "node": p.get("node"), "name": p.get("name"), "cmd": p.get("cmd")}
        row.update({f: p.get(f, 0) - q.get(f, 0) for f in _PANE_COUNTER_FIELDS})
        rows.append(row)
    born = sorted(set(newer_panes) - set(older_panes)) if not restart else sorted(newer_panes)
    gone = sorted(set(older_panes) - set(newer_panes)) if not restart else sorted(older_panes)
    return {
        "status": "ok",
        "rows": rows,
        "born": born,
        "gone": gone,
        "session": session,
        "window_s": window_s,
    }


def _render_pane_stats_lines(section: dict) -> list[str]:
    """The human-readable per-pane counter block. Scope-stated even when the
    news is "cannot say": one sample or an unreadable journal prints its
    status line, never an empty table that reads as 'no cost'."""
    out = ["pane counters (mux server; monotonic totals differenced over the window)"]
    if section["status"] != "ok":
        reason = {
            "insufficient-samples": "insufficient samples: need two mux_pane_counters "
            "events in the journal (the mux emits one per 30s while panes live)",
            "unreadable": f"journal unreadable: {section.get('error', 'unknown error')}",
        }.get(section["status"], section["status"])
        out.append(reason)
        return out
    if section.get("window_s") is not None:
        out.append(f"session {section['session']} | window {section['window_s']}s")
    out.append(
        f"{'PANE':>5} {'NODE':<11} {'NAME':<16} {'BYTES_IN':>10} "
        f"{'GRIDS':>8} {'COMPOSITED':>10} {'EMITTED':>8} {'CPU_MS':>9}"
    )
    if not section["rows"]:
        out.append("no pane appeared in both samples")
    for r in section["rows"]:
        out.append(
            f"{r['pane_id']:>5} {str(r['node'] or '-'):<11} {str(r['name'] or '-'):<16} "
            f"{r['bytes_in']:>10} {r['grid_updates']:>8} {r['frames_composited']:>10} "
            f"{r['frames_emitted']:>8} {r['cpu_ns'] / 1_000_000:>9.1f}"
        )
    if section["born"]:
        out.append(f"born this window: {', '.join(map(str, section['born']))}")
    if section["gone"]:
        out.append(f"gone this window: {', '.join(map(str, section['gone']))}")
    return out


def render_top(
    as_json: bool = False, include_subagents: bool = False, include_pane_stats: bool = False
) -> str:
    """Render the union table (or its JSON mirror - same rows, LD: parity).

    ``include_subagents`` appends a read-only sidechain section (x-af92); in
    JSON it adds a ``subagents`` key and folds the scan warnings in.
    ``include_pane_stats`` appends the per-pane mux counter deltas (one reader,
    :func:`pane_counter_rows`).
    """
    c = census()
    rows = _rows(c.workers, _crown_map())
    subagents = _subagent_section() if include_subagents else None
    pane_stats = pane_counter_rows() if include_pane_stats else None
    if as_json:
        payload: dict = {
            "workers": rows,
            "slot_claims": c.slot_claims,
            "warnings": list(c.warnings),
        }
        if subagents is not None:
            payload["subagents"] = subagents["rows"]
            payload["warnings"] = c.warnings + subagents["warnings"]
        if pane_stats is not None:
            payload["pane_stats"] = pane_stats
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
    if pane_stats is not None:
        out.append("")
        out.extend(_render_pane_stats_lines(pane_stats))
    return "\n".join(out)
