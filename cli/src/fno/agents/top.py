"""``fno agents top`` (x-c5cc): every live worker process with tree RSS.

One table over the SAME union the spawn gate counts (``spawn_gate.census``,
never duplicated), so the debugging surface and the enforcement surface can
never disagree. Python-only by design (LD8). The cost column is the worker's
whole process TREE off the resolved session pid (x-3f84 W2) - a recorded pid
alone prices the PTY host and misses the per-session MCP servers.
``--subagents`` (x-af92) appends a display-only sidechain section: never
slot-counted, observable, not addressable.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import NamedTuple, Optional

from fno.agents.discover import (
    _SUBAGENT_SCAN_WINDOW_S,
    _subagent_live_seconds,
    discover_subagents,
)
from fno.agents.session_procs import tree_rss_mb
from fno.agents.spawn_gate import LiveWorker, census


def lane_rows() -> list[dict]:
    """Per-provider lane occupancy against the cap that actually refuses.

    The constraint that governs this fleet was invisible: measured 2026-09-01
    the zai lane cap was binding (7 live rows, a spawn refused) while every
    machine-capacity surface said "plenty of room". Counted by the gate's OWN
    functions (:func:`provider_live_count`, :func:`provider_lanes_cap`),
    never a second registry walk - a display that recounts disagrees with the
    refusal the first time either changes. A count that cannot be read is
    reported as unreadable, NEVER as 0: the gate treats the unreadable case
    as a refusal (fail-closed), so an empty fleet would invert the meaning.
    Providers appear when capped or when a live row names them (`cap: None`).
    """
    from fno.agents.spawn_gate import (
        LIVE_STATUSES,
        ProviderCountUnavailable,
        provider_lanes_cap,
        provider_live_count,
    )

    try:
        from fno.config import load_settings, provider_limits_table

        agents_cfg = load_settings().agents
        limits = dict(provider_limits_table(agents_cfg))
    except Exception as exc:  # noqa: BLE001 - an unreadable config is reported
        return [{"provider": None, "unreadable": f"config unreadable: {exc}"}]

    # Which providers to ASK about. A row's mere presence is enough to raise the
    # question; whether it OCCUPIES a lane is the counter's answer, not this
    # set's. Keeping the two apart is what stops an uncapped provider from
    # disappearing just because nothing counted for it.
    observed: set[str] = set()
    try:
        from fno.agents.registry import load_registry

        for row in load_registry():
            if row.status in LIVE_STATUSES and row.provider:
                observed.add(row.provider)
    except Exception:  # noqa: BLE001 - degrade to the configured providers
        pass

    out: list[dict] = []
    for provider in sorted(set(limits) | observed):
        cap = provider_lanes_cap(limits.get(provider))
        lane: dict = {"provider": provider, "cap": cap, "holders": []}
        counted: set[str] = set()
        try:
            lane["count"] = provider_live_count(provider, counted)
        except ProviderCountUnavailable as exc:
            lane["count"] = None
            lane["unreadable"] = str(exc)
        else:
            lane["full"] = cap is not None and lane["count"] >= cap
            # Holders come from the counter's own tally, never a second walk:
            # printing rows the count did not include reads as "0 of these 5".
            lane["holders"] = sorted(counted)
        out.append(lane)
    return out


def _render_lane_lines(rows: list[dict]) -> list[str]:
    """One LANES line per provider; the verdict word is the scannable part."""
    lines: list[str] = []
    for r in rows:
        if r.get("provider") is None:
            lines.append(f"LANES  {r['unreadable']}")
            continue
        cap = r["cap"]
        if r.get("count") is None:
            occupancy = f"?/{cap if cap is not None else '-'}"
            verdict = f"unreadable: {r['unreadable']}"
        else:
            occupancy = f"{r['count']}/{cap if cap is not None else '-'}"
            verdict = "FULL" if r.get("full") else ("ok" if cap is not None else "uncapped")
        line = f"LANES  {r['provider']:<9} {occupancy:>7}  {verdict}"
        if r["holders"]:
            line += f"  holders: {', '.join(r['holders'])}"
        lines.append(line)
    return lines


def _crown_map() -> dict[str, str]:
    """name -> crown label for crowned registry rows (US9), sourced from
    :func:`crown_reading` so this view and ``fno whoami`` cannot drift.
    Best-effort: a read failure degrades to no crowns."""
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


def _registry_maps() -> tuple[dict[str, str], dict[str, Optional[str]]]:
    """One registry read feeding both session-id joins (x-1379): ``handles``
    is the session uuid -> handle bridge (a foreign claude row is labelled by
    the FIRST 8 hex of that uuid, the registry handle is the LAST 8 - the
    mismatch that once read as "all agents are dead"), ``nodes`` is the
    handle -> node map the retirement verdict resolves through. Best-effort,
    like :func:`_crown_map`: a read failure degrades to empty maps."""
    try:
        from fno.agents.registry import load_registry

        handles: dict[str, str] = {}
        nodes: dict[str, Optional[str]] = {}
        for e in load_registry():
            if e.harness_session_id and e.name:
                handles[e.harness_session_id] = e.name
            if e.name:
                nodes[e.name] = e.node
        return handles, nodes
    except Exception:  # noqa: BLE001 — top is a debug view, never fail on it
        return {}, {}


class RowTruth(NamedTuple):
    """What one transcript read says about a census row (x-6d89): ``reach``
    is the verdict the old ``_progress_map`` computed and threw away.
    ``progress`` is None exactly when the row has no registry entry, which
    alone lacks the harness/route context a refusal verdict needs."""

    progress: Optional[str]
    activity: str
    age_s: Optional[float]
    reach: Optional[str]
    reach_basis: Optional[str]


def _row_truth(workers: list[LiveWorker]) -> dict[str, RowTruth]:
    """name -> :class:`RowTruth`, one transcript read per row, every row:
    progress and the reachability verdict beside RSS (the surface that once
    showed 8513 MB across 31 live pids with no way to see which were
    parked), from ONE read, the shape ``fno.agents.read`` uses (x-6d89)."""
    from fno.agents.reachability import (
        classify_progress,
        classify_reachability,
        registry_falsifier,
        rendered_activity,
    )
    from fno.agents.registry import load_registry
    from fno.agents.session_truth import resolve_session_truth

    by_name: dict = {}
    by_session: dict = {}
    try:
        for e in load_registry():
            by_name[e.name] = e
            if e.harness_session_id:
                by_session[e.harness_session_id] = e
    except Exception:  # noqa: BLE001 — top is a debug view, never fail on it
        pass

    out: dict[str, RowTruth] = {}
    for w in workers:
        # The session uuid joins first (x-1379): the registry keys a foreign
        # claude row by its handle, not by this view's first-8-hex label.
        entry = by_session.get(w.session_id or "")
        if entry is None:
            entry = by_name.get(w.name)
        truth = resolve_session_truth(w.name)
        truth_state = truth.get("state")
        reach = classify_reachability(
            truth_state=truth_state,
            age_s=truth.get("last_activity_age_s"),
            falsifier=registry_falsifier(entry) if entry is not None else None,
        )
        activity = rendered_activity(
            truth_state=truth_state,
            age_s=reach.age_s,
            reachability=reach.verdict,
        )
        if entry is None:
            out[w.name] = RowTruth(None, activity, reach.age_s, reach.verdict, reach.basis)
            continue
        prog = classify_progress(
            truth_state=truth_state,
            reachability=reach.verdict,
            observed_model=truth.get("observed_model"),
            harness=w.harness,
            route_settings_path=entry.route_settings_path,
            last_activity_age_s=truth.get("last_activity_age_s"),
        )
        out[w.name] = RowTruth(
            prog.verdict, activity, reach.age_s, reach.verdict, reach.basis
        )
    return out


def _rows(workers: list[LiveWorker], crowns: dict[str, str]) -> list[dict]:
    handles, reg_nodes = _registry_maps()
    truth_map = _row_truth(workers)
    # One retirement read for the whole roster (x-1379), keyed by the
    # REGISTRY identity: the first-8-hex census label resolves no node.
    from fno.agents.retirement import verdicts

    registry_ids = [handles.get(w.session_id or "") or w.name for w in workers]
    verdict_map = verdicts(
        (idn, reg_nodes.get(idn)) for idn in registry_ids
    )
    rows = []
    for w in workers:
        # A foreign claude row (no registry entry) still gets age and reach;
        # only the PROGRESS verdict needs the entry's harness/route context.
        row_truth = truth_map.get(w.name)
        activity = row_truth.activity if row_truth else w.status
        age = row_truth.age_s if row_truth else None
        # Null when this session has no registry row (a foreign claude session
        # that fno never adopted), which is a real answer, not a lookup miss.
        handle = handles.get(w.session_id or "")
        reg_name = handle or w.name
        v = verdict_map.get(reg_name)
        rows.append(
            {
                "source": w.source,
                "name": w.name,
                "handle": handle if handle != w.name else None,
                # HARNESS, not PROVIDER (the CLI, never the model vendor).
                "harness": w.harness,
                "substrate": w.substrate,
                # The king that spawned this worker (x-3f84 W4): which king
                # owns the cost; None for operator-run / legacy rows.
                "king": (w.spawned_by or "")[:8] or None,
                # The process that IS the session (x-3f84 W2): a bg row's
                # recorded pid names the PTY HOST, not the worker.
                "pid": w.session_pid or w.pid,
                "rss_mb": tree_rss_mb(w.session_pid or w.pid),
                # (x-c672, AC7) Served activity from the one truth read the
                # progress axis uses; the stored token rides `stored_status`.
                "status": activity,
                "status_age_s": age,
                "stored_status": w.status,
                # (x-d401) Why `stored_status` is not the registry's token.
                "status_basis": w.status_basis,
                # The orthogonal axis beside `status`: null for a foreign
                # claude row this view has no harness/route context to judge.
                "progress": row_truth.progress if row_truth else None,
                # x-6d89: the verdict the row always computed and never
                # showed, with the basis that says which question it answered.
                "reach": row_truth.reach if row_truth else None,
                "reach_basis": row_truth.reach_basis if row_truth else None,
                # x-1379: has this worker's node already shipped. Null node is
                # a real answer (unresolvable name), never a lookup miss.
                "node": v.node if v else None,
                "node_basis": v.node_basis if v else None,
                "retire": v.retire if v else False,
                "retire_reason": v.reason if v else None,
                "crown": crowns.get(reg_name),  # US9: null when uncrowned
            }
        )
    # Heaviest first: the row the operator is looking for when RAM is tight.
    rows.sort(key=lambda r: -float(r["rss_mb"] or 0))
    return rows


def _run_ended_rows(crowns: dict[str, str]) -> list[dict]:
    """Registry rows whose RUN ended but whose session still answers (x-74aa).

    census() counts runs holding a process, so a parked row drops out of the
    table while its transcript keeps moving, and absence licensed a second
    writer onto a live worktree. Display only: never enters LiveCensus. Only
    a positive UNREACHABLE verdict drops a row - absence of evidence stays.
    """
    from fno.agents.reachability import UNREACHABLE, classify_reachability, registry_falsifier
    from fno.agents.registry import load_registry
    from fno.agents.session_truth import resolve_session_truth
    from fno.agents.spawn_gate import LIVE_STATUSES

    try:
        entries = load_registry()
    except Exception:  # noqa: BLE001 — top is a debug view, never fail on it
        return []
    rows: list[dict] = []
    for e in entries:
        if e.status in LIVE_STATUSES:
            continue
        truth = resolve_session_truth(e.name)
        reach = classify_reachability(
            truth_state=truth.get("state"),
            age_s=truth.get("last_activity_age_s"),
            falsifier=registry_falsifier(e),
        )
        if reach.verdict == UNREACHABLE:
            continue
        rows.append(
            {
                "source": "registry",
                "name": e.name,
                "harness": e.harness,
                "substrate": getattr(e, "substrate", None) or "-",
                "king": (getattr(e, "spawned_by", None) or "")[:8] or None,
                "pid": None,
                "reach": reach.verdict,
                "reach_basis": reach.basis,
                "status": "run-ended",
                "status_age_s": reach.age_s,
                "stored_status": e.status,
                "status_basis": reach.basis,
                "crown": crowns.get(e.name),
            }
        )
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
    """Read-only sidechain rows for the --subagents section (x-af92): the
    rendered rows, any scan warning, and the live threshold for the header."""
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
    """Difference the last two ``mux_pane_counters`` snapshots in the journals.

    THE one reader for per-pane mux counters: ``fno agents top --pane-stats``
    renders it here and the spawn gate's pane-vs-bg-session pricing imports
    this same function. The mux emits monotonic TOTALS, never rates. Both
    the main and the ``.ephemeral`` sibling journal (retention routing,
    x-add3) are scanned, oldest first. Returns ``{status, rows, born, gone,
    session, window_s}``; a broken journal is ``unreadable``, never an empty
    table that reads as "no cost". Samples are grouped by mux session and
    differenced within the journal-latest one; a DECREASE means the server
    restarted on the same socket name: report born-and-gone, never negative.
    """
    from fno.events import EPHEMERAL_SUFFIX
    from fno.paths import global_events_json

    path = events_path if events_path is not None else global_events_json()
    sibling = path.with_name(path.name + EPHEMERAL_SUFFIX)
    empty: dict = {
        "status": "insufficient-samples",
        "rows": [],
        "born": [],
        "gone": [],
        "session": None,
        "window_s": None,
    }
    samples: list = []
    # Oldest file first so a session spanning the routing deploy or a sibling
    # rotation keeps chronological within-session order.
    for candidate in (
        path,
        sibling.with_name(sibling.name + ".1"),
        sibling,
    ):
        try:
            size = candidate.stat().st_size
            with candidate.open("rb") as fh:
                if size > _PANE_COUNTERS_TAIL_BYTES:
                    fh.seek(size - _PANE_COUNTERS_TAIL_BYTES)
                    fh.readline()  # drop the partial line the seek landed in
                tail = fh.read().decode("utf-8", errors="replace")
        except FileNotFoundError:
            continue  # a missing candidate = no samples from it, not a broken read
        except OSError as exc:
            return {**empty, "status": "unreadable", "error": f"{type(exc).__name__}: {exc}"}
        for line in tail.splitlines():
            if '"mux_pane_counters"' not in line:
                continue  # cheap pre-filter: the journal carries many types
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if ev.get("type") == "mux_pane_counters":
                samples.append(ev)

    if len(samples) < 2:
        return empty  # also the no-journal-at-all case: no samples, not a broken read

    # Group by mux session before differencing: the global journal interleaves
    # every live session's 30s rows, so "the last two samples journal-wide"
    # commonly crosses sessions and reads one session's restart as another's
    # data. The session whose newest sample is journal-latest is the one shown.
    by_session: dict = {}
    for ev in samples:
        by_session.setdefault(ev.get("data", {}).get("session"), []).append(ev)
    session_events = max(by_session.values(), key=lambda evs: evs[-1]["ts"])
    if len(session_events) < 2:
        return empty
    older, newer = session_events[-2], session_events[-1]

    def _pane_map(ev: dict) -> dict:
        # A row without an integer pane_id is journal noise: skip it rather
        # than KeyError the whole table.
        return {
            p["pane_id"]: p
            for p in ev.get("data", {}).get("panes", [])
            if isinstance(p, dict) and isinstance(p.get("pane_id"), int)
        }

    older_panes = _pane_map(older)
    newer_panes = _pane_map(newer)
    session = newer.get("data", {}).get("session")

    def _ts_seconds(ev: dict) -> Optional[float]:
        try:
            from datetime import datetime

            return datetime.fromisoformat(ev["ts"].replace("Z", "+00:00")).timestamp()
        except Exception:
            return None

    t0, t1 = _ts_seconds(older), _ts_seconds(newer)
    window_s = round(t1 - t0, 1) if t0 is not None and t1 is not None else None

    rows = []
    born: list = []
    gone: list = []
    for pid, p in sorted(newer_panes.items()):
        q = older_panes.get(pid)
        if q is None:
            born.append(pid)  # first sample this pane appears in
            continue
        if any(p.get(f, 0) < q.get(f, 0) for f in _PANE_COUNTER_FIELDS):
            # Totals only go up within one server incarnation; a decrease
            # means the server restarted on the SAME socket name (the session
            # label is the socket stem, so it did not change) and this pane id
            # belongs to the NEW incarnation. Report the reset, never a
            # negative delta.
            born.append(pid)
            gone.append(pid)
            continue
        row = {"pane_id": pid, "node": p.get("node"), "name": p.get("name"), "cmd": p.get("cmd")}
        row.update({f: p.get(f, 0) - q.get(f, 0) for f in _PANE_COUNTER_FIELDS})
        rows.append(row)
    gone.extend(sorted(set(older_panes) - set(newer_panes)))
    return {
        "status": "ok",
        "rows": rows,
        "born": sorted(born),
        "gone": sorted(gone),
        "session": session,
        "window_s": window_s,
    }


def _render_pane_stats_lines(section: dict) -> list[str]:
    """The human-readable per-pane counter block: "cannot say" prints its
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


def _retirable_lines(rows: list[dict], lanes: list[dict]) -> list[str]:
    """One line per lane holder whose node already shipped (x-1379).

    The provider comes from the SAME ``lane_rows`` output the LANES block
    rendered, never recounted. A holder no lane names still gets its line:
    the verdict is the graph's, not the lane counter's.
    """
    holder_lane: dict[str, Optional[str]] = {}
    for lane_row in lanes:
        for h in lane_row.get("holders") or []:
            holder_lane[h] = lane_row.get("provider")
    out = []
    for r in rows:
        if not r.get("retire"):
            continue
        # The lane counter tallies REGISTRY handles; join through the handle.
        provider = holder_lane.get(r.get("handle") or r["name"])
        holds = f" holds a {provider} lane" if provider else " holds a lane"
        pr = (r["retire_reason"] or "").rsplit(" ", 1)[-1]
        merged = f" at PR {pr}" if pr.isdigit() else ""
        out.append(f"retirable: {r['name']}{holds}; {r['node']} is done, merged{merged}")
    return out


def render_top(
    as_json: bool = False, include_subagents: bool = False, include_pane_stats: bool = False
) -> str:
    """Render the union table (or its JSON mirror - same rows, LD: parity).
    ``include_subagents`` appends the sidechain section (x-af92);
    ``include_pane_stats`` appends the per-pane mux counter deltas."""
    c = census()
    crowns = _crown_map()
    rows = _rows(c.workers, crowns)
    run_ended = _run_ended_rows(crowns)
    lanes = lane_rows()
    subagents = _subagent_section() if include_subagents else None
    pane_stats = pane_counter_rows() if include_pane_stats else None
    predicate = (
        "rows are RUNS holding a process (census LIVE_STATUSES); a session "
        "whose run ended is under run_ended, not missing; per-session "
        "liveness is fno agents truth <handle>"
    )
    if as_json:
        payload: dict = {
            "workers": rows,
            "run_ended": run_ended,
            "predicate": predicate,
            "lanes": lanes,
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
    # Lanes lead: a provider cap refuses spawns the table below calls healthy.
    if lanes:
        out.extend(_render_lane_lines(lanes))
        out.append("")
    # The retirable line leads with the lanes (x-1379): the same shape of
    # fact as a full lane - a cap refusing spawns the table calls healthy.
    retirable = _retirable_lines(rows, lanes)
    if retirable:
        out.extend(retirable)
        out.append("")
    header = (
        f"{'SOURCE':<7} {'NAME':<24} {'HARNESS':<9} {'SUBSTRATE':<10} "
        f"{'KING':<9} {'PID':>7} {'RSS_MB':>7} {'NODE':<8} {'PROGRESS':<17} "
        f"{'REACH':<11} STATUS"
    )
    out.append(header)
    if not rows:
        out.append("no live workers (runs holding a process; a run-ended session is not missing)")
    for r in [*rows, *run_ended]:
        # US9: mark a crowned worker in the name cell (ASCII, alignment-safe).
        # The registry handle rides along when it differs from this view's own
        # label, so `top` and `list` can be joined by eye instead of by guessing
        # which end of the uuid each one truncated.
        name_cell = r["name"] + (f" [{r['crown']}]" if r["crown"] else "")
        if r.get("handle"):
            name_cell += f" ={r['handle']}"
        age_s = r.get("status_age_s")
        activity = r["status"] + (f" {_fmt_age(age_s)}" if age_s is not None else "")
        out.append(
            f"{r['source']:<7} {name_cell:<24} {r['harness']:<9} "
            f"{r['substrate']:<10} {r['king'] or '-':<9} {r.get('pid') or '-':>7} "
            f"{r['rss_mb'] if r.get('rss_mb') is not None else '-':>7} "
            f"{r.get('node') or '-':<8} "
            f"{r.get('progress') or '-':<17} {r['reach'] or '-':<11} {activity}"
            + (f" ({r['status_basis']})" if r.get("status_basis") else "")
        )
    if c.slot_claims:
        out.append(f"(+{c.slot_claims} queued headless slot claim(s))")
    out.append(f"census: {predicate}. PID/RSS are the process at scan time; "
               "REACH reads the transcript (fno agents truth for the full "
               "evidence); NODE and the retirement line read the graph")
    if subagents is not None:
        out.append("")
        out.extend(subagents["warnings"])
        out.extend(_render_subagent_lines(subagents))
    if pane_stats is not None:
        out.append("")
        out.extend(_render_pane_stats_lines(pane_stats))
    return "\n".join(out)
