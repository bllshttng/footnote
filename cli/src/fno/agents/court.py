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


def gather_court(rows: Optional[list] = None) -> dict[str, Any]:
    """The whole court: every crown, its verdict, and any territorial conflict.

    ``rows`` overrides the live registry read for callers that already hold
    it (tests). An unreadable REGISTRY nulls ``crowns`` and every summary
    count rather than reporting an empty court: a caller gating on
    ``summary.disagreements == 0`` must not read a healthy fleet from a read
    that saw nothing.
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
    by_id = _graph_index()
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


def fold_scope_nodes(crowns: list[dict[str, Any]]) -> dict[str, Any]:
    """Fold each crown's scope onto its row via `fno-agents court-fold`; any
    fault marks the crown unresolved (design: docs/architecture/court-scope-fold.md).

    Returns the fold's own stuck verdict, computed beside the rows it judges
    so no second reader can disagree about what a row means. A fault answers
    ``{}``, and the unresolved crowns above carry the reason.
    """
    import json as _json
    import subprocess

    from fno.paths import graph_json
    from fno.rust_binary import resolve_binary

    if not crowns:
        return {}
    for crown in crowns:
        scope = crown.get("scope")
        if not (isinstance(scope, str) and scope.strip()) or crown.get("level") is None:
            crown["scope_nodes"] = {
                "status": "unresolved",
                "reason": "the row carries no scope or no crown level",
            }
    payload = [
        {"scope": c.get("scope"), "level": c.get("level")}
        for c in crowns
        if "scope_nodes" not in c
    ]
    if not payload:
        return {}
    try:
        binary = resolve_binary()
        if binary is None:
            raise OSError("the fno-agents binary was not found")
        proc = subprocess.run(
            [str(binary), "court-fold", "--graph", str(graph_json()),
             "--crowns-json", _json.dumps(payload), "--format", "json"],
            capture_output=True, text=True, check=False, timeout=30,
        )
        if proc.returncode != 0:
            raise RuntimeError(proc.stderr.strip() or f"exit {proc.returncode}")
        folded = _json.loads(proc.stdout)
        scope_nodes = folded["scope_nodes"]
    except Exception as exc:  # noqa: BLE001 - a failed fold is stated, never a crash
        # A TimeoutExpired stringifies to its whole argv, which buries the fault
        # it reports. Name that one and its bound instead.
        reason = (
            "the fold timed out after 30s"
            if isinstance(exc, subprocess.TimeoutExpired)
            else f"the fold could not run: {exc}"
        )
        for crown in crowns:
            crown.setdefault("scope_nodes", {"status": "unresolved", "reason": reason})
        return {}
    for crown in crowns:
        scope = crown.get("scope")
        crown["scope_nodes"] = scope_nodes.get(
            scope,
            {"status": "unresolved", "reason": "the fold answered no row for this scope"},
        )


def _gate_read() -> dict[str, Any]:
    """The spawn gate's verdict: `unknown` with a reason, never a healthy default."""
    try:
        from fno.agents.spawn_gate import probe_capacity

        verdict = probe_capacity()
    except Exception as exc:  # noqa: BLE001 - a display read never raises
        return {"verdict": "unknown", "reason": f"the gate could not be read: {exc}"}
    if not isinstance(verdict, dict) or not verdict.get("verdict"):
        return {"verdict": "unknown", "reason": "the gate answered no verdict"}
    return verdict


def _annotate_sessions(crowns: list[dict[str, Any]]) -> bool:
    """Judge each node row's session ids against the registry; returns whether it read.

    A uuid a reader cannot judge invites a confident wrong inference in both
    directions. Absent from the registry answers ``live: None``, never ``False``.
    """
    from fno.agents.registry import TERMINAL_STATUSES, load_registry

    try:
        rows, readable = load_registry(), True
    except Exception:  # noqa: BLE001 - an unreadable registry judges nothing
        rows, readable = [], False
    status = {
        sid: r.status for r in rows if (sid := getattr(r, "harness_session_id", None))
    }
    for crown in crowns:
        fold = crown.get("scope_nodes")
        if not isinstance(fold, dict):
            continue
        for node in fold.get("nodes") or []:
            node["sessions"] = [
                {
                    "id": sid,
                    "live": None if sid not in status else status[sid] not in TERMINAL_STATUSES,
                    "status": status.get(sid),
                }
                for sid in (node.get("sessions") or [])
                if isinstance(sid, str)
            ]
    return readable


def _blind_stuck(reason: str) -> dict[str, Any]:
    """The verdict when the fold itself could not answer, with the reason."""
    return {
        "unclaimed": [], "blocked": [], "unproven_claim": [], "in_review": [],
        "blind": [reason], "threshold_minutes": None,
    }


def _stuck_render(summary: dict[str, Any], gate: dict[str, Any]) -> str:
    """One line. A clean read and a blind read must never look the same."""
    line = summary.get("stuck_line") or ""
    parts = [line] if line else []
    if gate.get("verdict") == "refused":
        parts.append(f"gate refused {gate.get('reason')}")
    # The fold already rendered its own blind reasons into `line`; only the
    # ones this caller added still need a clause.
    parts += [f"could not answer: {r}" for r in summary["stuck"]["blind"] if r not in line]
    return "stuck: " + (", ".join(parts) if parts else "nothing")


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


def render_court(as_json: bool, nodes: bool = False) -> str:
    """The full render: table + conflicts + summary, or its JSON mirror.

    ``nodes`` folds each crown's scope into its row (the native read) and
    always answers JSON - the fold's row data is tabular, and the board's
    court section is its human view. A fold that cannot run marks the crown
    unresolved rather than rendering an empty table.
    """
    import json

    court = gather_court()
    if court["crowns"]:
        # Fold for EVERY render shape. The plain table is what an operator
        # types, and a stuck line it cannot compute is the gap this read
        # exists to close. The rows are dropped again below unless -n asked
        # for them, so a bare --json keeps the contract its callers pin.
        folded = fold_scope_nodes(court["crowns"])
        court["sessions_readable"] = _annotate_sessions(court["crowns"])
        court["gate"] = _gate_read()
        # The fold judges its own rows; only the gate half is the caller's. A
        # fold that answered rows but no verdict is a binary older than this
        # court, a different fault from one that never ran.
        stuck = folded.get("stuck") or _blind_stuck(
            "the scope fold did not run"
            if not folded
            else "the fno-agents binary predates this court and returned no "
            "stuck verdict; run fno doctor --fix"
        )
        if court["gate"].get("verdict") == "unknown":
            stuck["blind"].append(
                f"the spawn gate answered unknown: {court['gate'].get('reason')}"
            )
        court["summary"]["stuck"] = stuck
        court["summary"]["stuck_line"] = folded.get("stuck_line", "")
        if not nodes:
            for crown in court["crowns"]:
                crown.pop("scope_nodes", None)
    if as_json or nodes:
        return json.dumps(court, indent=2, sort_keys=True)

    if court["crowns"] is None:
        return f"court: CANNOT READ - {court['summary']['reason']}. This is not an empty court; nothing was checked."
    if not court["crowns"]:
        return "court: no live crowns"

    header = f"{'SCOPE':<16} {'LEVEL':<5} {'HOLDER':<20} {'GRANTOR':<16} {'STATUS':<14} AGREE SOURCE"
    lines = [header] + [_fmt_row(e) for e in court["crowns"]]
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
    if isinstance(s.get("stuck"), dict):
        lines.append(_stuck_render(s, court.get("gate") or {}))
    return "\n".join(lines)


def register_court_command(app) -> None:
    """Attach the court command to the agents app. The body lives here, next
    to the read it serves; the composition stays on the CLI surface."""
    import typer

    @app.command("court", hidden=True)
    def cmd_court(
        json_output: bool = typer.Option(
            False, "--json", "-J", help="Emit JSON instead of the table."
        ),
        nodes: bool = typer.Option(
            False,
            "--nodes",
            "-n",
            help=(
                "Fold each crown's scope nodes into its row (counts by status, "
                "then the active nodes with worker, PR, session ids). Implies "
                "JSON output."
            ),
        ),
    ) -> None:
        """The whole court: every live crown, its scope, its holder, its
        grantor, and whether the registry and the graph agree.

        Exit 0 always: this is a read, and a caller gates on the JSON keys
        (``agree``, ``summary.disagreements``, ``summary.unknowns``, and
        ``conflicts``), not the process status. Two live rows holding one
        territory can each report ``agree: true`` while the fleet has two
        kings over one scope, so ``conflicts`` is part of every gate read -
        the precise failure this command exists to end.
        """
        from fno.agents.court import render_court

        print(render_court(json_output, nodes=nodes))
