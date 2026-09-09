"""``fno agents court``: one read of every crown, its scope, its holder, and
whether the registry and the graph agree - the read that would have prevented
the 2026-08-20 incident, when a three-crown re-scope took five attempts
because no single command showed the rows still reading the old scope.

Agreement is a POSITIVE marker: ``agree`` is ``True`` only when the graph was
read and the scope checked out; an unreadable graph answers ``None`` with a
stated reason, and the summary counts unknowns separately. Each crown also
names its manifest limb (path, session, ``crown_source``) - the manifest is
the durable crown record, the row its cache.
"""
from __future__ import annotations

from pathlib import Path
from typing import Any, Optional

from fno.agents.crown import (
    _canonical_project,
    _crown_rivals,
    _graph_index,
    _territory_key,
    crown_reading,
    split_scope,
)
from fno.plan._status import TERMINAL_STATUSES as PLAN_TERMINAL_STATUSES


def _id_index(entries: list[dict]) -> dict[str, dict]:
    """Graph entries keyed by id - the one builder agreement and fold share."""
    return {
        entry["id"]: entry
        for entry in entries
        if isinstance(entry, dict) and isinstance(entry.get("id"), str) and entry["id"]
    }


def _agreement(
    level: Optional[int], scope: Optional[str], by_id: Optional[dict[str, dict]]
) -> tuple[Optional[bool], Optional[str]]:
    """Does the graph corroborate this crown? ``(agree, reason)``.

    An unadjudicable crown answers ``(None, reason)``, never ``True`` or
    ``False``; only the epic rung needs the graph.
    """
    members = split_scope(scope)
    # A scope-less level is half a crown: it rules no territory and must not
    # fall through to the emptiness-blind checks below.
    if not members:
        return False, "the row carries a crown level but no scope (half a crown)"
    if level == 2:
        if by_id is None:
            return None, "graph unreadable"
        # A rung-2 scope is a SET of epics; every member must be a live epic,
        # so one dead member makes the whole crown disagree - not just the
        # first member an earlier cut checked.
        for node_id in members:
            entry = by_id.get(node_id)
            if entry is None:
                return False, f"{node_id!r} is not in the graph"
            node_type = entry.get("type")
            if node_type != "epic":
                return False, f"{node_id!r} is a {node_type or 'node'}, not an epic"
            status = entry.get("status")
            if status in PLAN_TERMINAL_STATUSES:
                return False, f"{node_id!r} status is {status!r} (terminal)"
        return True, None
    # Level 0/1: agrees when every member resolves to a configured project,
    # the same check `resolve_crown` made at grant time.
    unresolved = [m for m in members if _canonical_project(m) is None]
    if unresolved:
        return False, (
            f"{', '.join(unresolved)} not a configured project"
            if len(unresolved) == 1
            else f"{', '.join(unresolved)} are not configured projects"
        )
    return True, None


def _conflicts(rows: list) -> list[dict[str, Any]]:
    """Territory two live crowned rows double-rule, one entry per rival PAIR.

    Keys on :func:`_crown_rivals`, the rule the grant-time holder scan uses,
    so a conflict here and the refusal at grant time cannot disagree: a
    set-holder rivals a holder over one member; a portfolio and the project
    kings of its court are two legitimate crowns. One entry PER PAIR, never a
    merged group: rivalry is not transitive (A/e-1, B/e-1,e-2, C/e-2 rivals
    A-B and B-C only), so a group would claim three rows hold what no pair
    does. Each entry names its two rows and the members they actually share.
    """
    # Joined on crown_scope, not a full crown_reading: a scope claims territory
    # with or without a level, and gather_court surfaces those rows too.
    claims: list[tuple[Any, str, frozenset[str]]] = []
    for row in rows:
        scope = getattr(row, "crown_scope", None)
        if isinstance(scope, str) and scope.strip():
            key = _territory_key(scope)
            if key:
                claims.append((row, scope, key))
    conflicts: list[dict[str, Any]] = []
    for i in range(len(claims)):
        for j in range(i + 1, len(claims)):
            row_i, scope_i, key_i = claims[i]
            row_j, scope_j, key_j = claims[j]
            if not _crown_rivals(
                scope_i,
                getattr(row_i, "crown_level", None),
                scope_j,
                getattr(row_j, "crown_level", None),
            ):
                continue
            conflicts.append(
                {
                    "scope": ",".join(sorted(key_i & key_j)),
                    "holders": [row_i.name, row_j.name],
                }
            )
    return conflicts


def _manifest_limb(scope: Any, row: Any) -> dict[str, Any]:
    """The manifest side of one crown; ``reign_state`` is the single comparator."""
    from fno.king.state import king_state_root, reign_state

    limb: dict[str, Any] = {"manifest_path": None, "manifest_session": None, "crown_source": "row"}
    cwd = getattr(row, "cwd", None)
    if not (isinstance(scope, str) and scope.strip() and isinstance(cwd, str) and cwd.strip()):
        return limb
    try:
        state = reign_state(scope, state_root=king_state_root(Path(cwd)))
    except (OSError, ValueError):
        return limb
    limb["manifest_session"], limb["manifest_path"] = state.manifest_session, state.manifest_path
    if state.split is True:
        limb["crown_source"] = "split"
    elif state.crown_on_manifest is True:
        limb["crown_source"] = "both"
    return limb


def _manifest_only_crowns(held: list[str]) -> tuple[list[dict[str, Any]], bool]:
    """Crowns whose row is gone but whose manifest holds them: the Rust sweep
    (`fno-agents court-orphans`) walks the spaces ROOT because a vanished row
    names no cwd. Returns ``(entries, ran)``: ``ran`` False means the sweep
    could not answer, so an empty list is an ABSENCE, never zero orphans."""
    import json
    import subprocess

    from fno.paths import spaces_root
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        return [], False
    try:
        proc = subprocess.run(
            [str(binary), "court-orphans", "--root", str(spaces_root())]
            + [part for scope in held for part in ("--held", scope)],
            capture_output=True, text=True, check=False, timeout=30,
        )
        if proc.returncode != 0:
            return [], False
        orphans = json.loads(proc.stdout)
    except (OSError, ValueError, subprocess.SubprocessError):
        return [], False
    entries = [
        {
            "holder": o.get("manifest_session") or o.get("scope"),
            "level": o.get("level"),
            "scope": o["scope"],
            "grantor": o.get("grantor") or "human",
            "status": "manifest-only",
            "agree": None,
            "reason": "crown lives on the manifest; no live registry row holds it",
            "manifest_path": o.get("manifest_path"),
            "manifest_session": o.get("manifest_session"),
            "crown_source": "manifest",
        }
        for o in orphans
        if o.get("scope")
    ]
    return entries, True


def find_presiding_crown(
    scope: str, level: Optional[int], crowns: list[dict[str, Any]], by_id: Optional[dict[str, dict]]
) -> Optional[dict[str, Any]]:
    """The live crown one rung above scope/level, or None (x-3ecf AC4-HP)."""
    if level is None or level <= 0:
        return None
    live = [c for c in crowns if c.get("status") != "manifest-only"]
    if level == 2:
        projects = {by_id.get(m, {}).get("project") for m in split_scope(scope)} if by_id else set()
        projects.discard(None)
        if len(projects) != 1:
            return None
        (proj,) = projects
        return next((c for c in live if c.get("level") == 1 and c.get("scope") == proj), None)
    if level == 1:
        return next(
            (c for c in live if c.get("level") == 0 and scope in split_scope(c.get("scope"))), None
        )
    return None


def gather_court(
    rows: Optional[list] = None, *, entries: Optional[list[dict]] = None
) -> dict[str, Any]:
    """The whole court: every crown, its verdict, and any territorial conflict.

    ``rows`` overrides the live registry read for callers that already hold
    it (tests). ``entries`` overrides the graph read for callers that already
    hold it (the ``--nodes`` fold and the HTML court section): building the
    id index from them skips ``_graph_index`` entirely, so one read serves
    agreement and fold together. An unreadable REGISTRY nulls ``crowns`` and
    every summary count rather than reporting an empty court: a caller
    gating on ``summary.disagreements == 0`` must not read a healthy fleet
    from a read that saw nothing.
    """
    from fno.agents.registry import TERMINAL_STATUSES, load_registry

    if rows is None:
        try:
            rows = load_registry()
        except Exception as exc:
            return {
                "crowns": None,
                "conflicts": None,
                "registry_readable": False,
                "graph_readable": None,
                "summary": {
                    "total": None,
                    "disagreements": None,
                    "unknowns": None,
                    "splits": None,
                    "reason": f"registry unreadable: {exc}",
                },
            }
    live_rows = [r for r in rows if r.status not in TERMINAL_STATUSES]

    # One graph parse for every rung; ``None`` (unreadable) is not "nothing here".
    if entries is None:
        by_id = _graph_index()
    else:
        by_id = _id_index(entries)
    crowns: list[dict[str, Any]] = []
    held_scopes: list[str] = []
    for row in live_rows:
        reading = crown_reading(row)
        if reading is None:
            # crown_reading returns None whenever crown_level is None,
            # regardless of crown_scope; surface the anomaly, never skip it.
            if getattr(row, "crown_scope", None):
                # The half crown still HOLDS its territory; _conflicts counts it as a claim.
                if isinstance(row.crown_scope, str) and row.crown_scope.strip():
                    held_scopes.append(row.crown_scope)
                crowns.append(
                    {
                        "holder": row.name,
                        "level": row.crown_level,
                        "scope": row.crown_scope,
                        "grantor": getattr(row, "crown_grantor", None) or "human",
                        "status": row.status,
                        "agree": False,
                        "reason": "half a crown: scope is set but level is missing",
                        **_manifest_limb(row.crown_scope, row),
                    }
                )
            continue
        agree, reason = _agreement(reading["level"], reading["scope"], by_id)
        crowns.append(
            {
                "holder": row.name,
                "level": reading["level"],
                "scope": reading["scope"],
                "grantor": reading["grantor"],
                "status": row.status,
                "agree": agree,
                "reason": reason,
                **_manifest_limb(reading["scope"], row),
            }
        )
        if isinstance(reading["scope"], str) and reading["scope"].strip():
            held_scopes.append(reading["scope"])

    orphans, sweep_ran = _manifest_only_crowns(held_scopes)
    crowns.extend(orphans)

    disagreements = sum(1 for e in crowns if e["agree"] is False)
    unknowns = sum(1 for e in crowns if e["agree"] is None)
    splits = sum(1 for e in crowns if e["crown_source"] == "split")
    return {
        "crowns": crowns,
        "conflicts": _conflicts(live_rows),
        "registry_readable": True,
        "graph_readable": by_id is not None,
        "summary": {
            # total counts ROW crowns only: the census computes workers from it.
            "total": len(crowns) - len(orphans),
            "manifest_only": len(orphans),
            "sweep_ran": sweep_ran,
            "disagreements": disagreements,
            "unknowns": unknowns,
            "splits": splits,
        },
    }


def fold_scope_nodes(crowns: list[dict[str, Any]], entries: list[dict]) -> None:
    """Fold each crown's scope nodes onto its row as ``scope_nodes``, in place.

    Pure over rows the caller already read: never a second graph read. The
    resolver is injected from the crown row's own
    ``level``/``scope`` because ``gather_court`` already adjudicated that
    scope (``agree``) - re-validating inside ``compile_scope_ids`` would buy
    nothing and re-read the graph (measured 9.5-13.4 s through the keeper).

    ``scope_nodes`` is ``{"status": "ok", "total", "counts", "nodes",
    "omitted"}`` or ``{"status": "unresolved", "reason"}``. ``omitted`` is
    always present on an ok fold: a crown whose active list is empty must
    read as "N nodes, none active", never as "nothing here".
    """
    from fno.claims.core import live_workers
    from fno.graph.statuses import ACTIVE_STATUSES
    from fno.king.scope import compile_scope_ids

    if not crowns:
        return
    by_id = _id_index(entries)
    # Counts render in lifecycle order; a status outside the vocabulary keeps
    # its place at the end rather than vanishing from the line.
    count_order = [
        "in_progress", "in_review", "ready", "blocked", "design",
        "idea", "deferred", "done", "superseded",
    ]
    # Pass 1: compile every crown and collect the active ids the worker read
    # will name.
    compiled: list[tuple[dict[str, Any], list[dict], Optional[dict[str, Any]]]] = []
    active_ids: list[str] = []
    for crown in crowns:
        scope = crown.get("scope")
        level = crown.get("level")
        if not (isinstance(scope, str) and scope.strip()) or level is None:
            compiled.append((
                crown,
                [],
                {
                    "status": "unresolved",
                    "reason": "the row carries no scope or no crown level",
                },
            ))
            continue
        try:
            ids = compile_scope_ids(
                scope, entries, resolve=lambda _m, level=level, scope=scope: (level, scope)
            )
        except (ValueError, KeyError) as exc:
            compiled.append((crown, [], {"status": "unresolved", "reason": str(exc)}))
            continue
        members = [by_id[i] for i in sorted(ids) if i in by_id]
        active_ids.extend(
            str(e["id"]) for e in members if e.get("status") in ACTIVE_STATUSES
        )
        compiled.append((crown, members, None))
    # Pass 2: ONE verdict batch, stat-filtered to the lockfiles that exist;
    # the per-key read pays one native verdict each and measured 1.7 s over
    # 122 active rows.
    workers = live_workers(list(dict.fromkeys(active_ids)))
    for crown, members, error in compiled:
        if error is not None:
            crown["scope_nodes"] = error
            continue
        counts: dict[str, int] = {}
        for entry in members:
            status = str(entry.get("status") or "unknown")
            counts[status] = counts.get(status, 0) + 1
        rows = []
        for entry in members:
            if entry.get("status") not in ACTIVE_STATUSES:
                continue
            sessions: list[str] = []
            for raw in entry.get("sessions") or []:
                sid = raw.get("session_id") if isinstance(raw, dict) else raw
                if sid and sid not in sessions:
                    sessions.append(sid)
            for raw in (
                [entry.get("session_id")]
                + list(entry.get("cost_sessions") or [])
                + [entry.get("locked_by_harness_session")]
            ):
                if raw and raw not in sessions:
                    sessions.append(raw)
            rows.append(
                {
                    "id": entry.get("id"),
                    "slug": entry.get("slug") or "",
                    "status": str(entry.get("status") or ""),
                    "worker": workers.get(str(entry["id"])),
                    "pr_number": entry.get("pr_number"),
                    "sessions": sessions,
                }
            )
        ordered = {k: counts[k] for k in count_order if k in counts}
        ordered.update(
            {k: counts[k] for k in sorted(counts) if k not in ordered}
        )
        crown["scope_nodes"] = {
            "status": "ok",
            "total": len(members),
            "counts": ordered,
            "nodes": rows,
            "omitted": len(members) - len(rows),
        }


def crowned_sessions(rows: list) -> set[str]:
    """The sessions that hold a crown, read the way ``gather_court`` reads.

    Same non-terminal rows, same ``crown_level`` field: a row is a king here
    iff it is a king in the court (x-5283 LD1). The spawn gate divides
    ``max_live`` by this set; callers guard readability themselves.
    """
    from fno.agents.registry import TERMINAL_STATUSES

    return {
        row.harness_session_id
        for row in rows
        if row.status not in TERMINAL_STATUSES
        and row.crown_level is not None
        and row.harness_session_id
    }


def _fmt_row(e: dict[str, Any]) -> str:
    agree = "?" if e["agree"] is None else ("yes" if e["agree"] else "no")
    reason = f"   {e['reason']}" if e["reason"] else ""
    # str() every cell: a null scope must not crash the render that surfaces it.
    return (
        f"{str(e['scope']):<16} {str(e['level']):<5} {str(e['holder']):<20} "
        f"{str(e['grantor']):<16} {str(e['status']):<14} {agree:<4} "
        f"{str(e.get('crown_source')):<8}{reason}"
    )


def _fold_lines(e: dict[str, Any]) -> list[str]:
    """The indented scope-fold block printed under one crown's table row."""
    sn = e.get("scope_nodes")
    if not sn:
        return []
    if sn["status"] == "unresolved":
        return [f"  scope fold: unresolved - {sn['reason']}"]
    counts = ", ".join(f"{k} {v}" for k, v in sn["counts"].items())
    lines = [
        f"  {sn['total']} node{'s' if sn['total'] != 1 else ''}: "
        f"{counts}   ({sn['omitted']} not listed)"
    ]
    lines.append(f"  {'NODE':<8} {'STATUS':<12} {'WORKER':<18} {'PR':<6} SESSIONS")
    for r in sn["nodes"]:
        pr = f"#{r['pr_number']}" if r.get("pr_number") else ""
        sessions = ", ".join(str(s) for s in r.get("sessions") or [])
        lines.append(
            f"  {str(r['id']):<8} {str(r['status']):<12} "
            f"{str(r.get('worker') or '-'):<18} {pr:<6} {sessions}"
        )
    return lines


def render_court(as_json: bool, nodes: bool = False) -> str:
    """The full render: table + conflicts + summary, or its JSON mirror.

    ``nodes`` folds each crown's scope into its row (``fold_scope_nodes``) off
    the ONE graph read this render performs; a failed read is stated in the
    output rather than rendered as a court of empty scopes.
    """
    import json

    entries = None
    if nodes:
        from fno.tracker.metadata import read_entries

        try:
            entries = read_entries("agents.court")
        except Exception:  # noqa: BLE001 - stated below, never a crash
            entries = None
    court = gather_court(entries=entries)
    if nodes and entries is not None and court["crowns"]:
        fold_scope_nodes(court["crowns"], entries)
    if as_json:
        return json.dumps(court, indent=2, sort_keys=True)

    if court["crowns"] is None:
        return f"court: CANNOT READ - {court['summary']['reason']}. This is not an empty court; nothing was checked."
    if not court["crowns"]:
        return "court: no live crowns"

    header = f"{'SCOPE':<16} {'LEVEL':<5} {'HOLDER':<20} {'GRANTOR':<16} {'STATUS':<14} AGREE SOURCE"
    if nodes:
        lines = [header]
        for e in court["crowns"]:
            lines.append(_fmt_row(e))
            lines.extend(_fold_lines(e))
    else:
        lines = [header] + [_fmt_row(e) for e in court["crowns"]]
    if nodes and entries is None:
        lines.append(
            "\nscope fold skipped: the graph read for the fold failed; "
            "crown rows carry no scope nodes rather than empty ones."
        )
    for c in court["conflicts"]:
        holders = ", ".join(c["holders"])
        lines.append(f"\nconflicts: scope {c['scope']!r} held by {len(c['holders'])} live rows ({holders})")
    s = court["summary"]
    lines.append(
        f"\ncourt: {s['total']} crown{'s' if s['total'] != 1 else ''}, "
        f"{s['disagreements']} disagreement"
        f"{'s' if s['disagreements'] != 1 else ''}, {s['unknowns']} unknown"
        f"{'s' if s['unknowns'] != 1 else ''}, {s['splits']} split"
        f"{'s' if s['splits'] != 1 else ''}"
        + (f", {s['manifest_only']} manifest-only" if s.get("manifest_only") else "")
    )
    if s.get("sweep_ran") is False:
        lines.append("orphan sweep did not run (stale or missing binary): zero manifest-only entries is an absence, not a finding")
    return "\n".join(lines)
