"""Rank a graph node and qualify whether a live mission can reach it."""

from __future__ import annotations

import math
from typing import Optional

import typer


def _dispatch_note(task_id: str, graph_path) -> str | None:
    """Return a truthful dispatcher note for a successfully ranked node."""
    try:
        from fno.active_backlog import resolve_drain_targets
        from fno.graph._intake import descendants_of
        from fno.graph.store import read_graph

        entries = read_graph(graph_path)
        if not isinstance(entries, list) or any(not isinstance(e, dict) for e in entries):
            raise ValueError("graph read returned an unreadable shape")
        missions: list[str] = []
        for target in resolve_drain_targets(strict=True):
            mission = getattr(target, "mission", None)
            if mission is None:
                continue
            if not isinstance(mission, str) or not mission:
                raise ValueError("active-backlog target has no readable mission")
            missions.append(mission)
        missions = sorted(set(missions))
        reachable = {
            child_id
            for mission in missions
            for child_id in descendants_of(entries, mission)
        }
        if task_id in reachable:
            return None
        # The remedy, not just the diagnosis (x-7f1f): the note names the one
        # command that makes a dispatcher take the node - activating its epic.
        # With no epic parent it says so, so the note never prints a command
        # that cannot work.
        me = next((e for e in entries if e.get("id") == task_id), None)
        parent = (me or {}).get("parent")
        if parent:
            remedy = f"; Activate its epic: fno backlog advance --epic {parent}"
        else:
            remedy = (
                "; no epic to activate (missions are activated per epic with "
                "fno backlog advance --epic <epic-id>)"
            )
        if missions:
            return (
                "no live dispatcher will take it "
                f"(outside active mission scopes: {', '.join(missions)})"
                + remedy
            )
        return "no live dispatcher will take it (no resolved active missions)" + remedy
    except Exception as exc:  # noqa: BLE001 - rank already committed; qualify unknowns
        return f"dispatcher scope unavailable ({exc})"


def cmd_rank(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX) to rank"),
    top: bool = typer.Option(False, "--top", help="Pin to the front of its (column, project) lane"),
    bottom: bool = typer.Option(
        False, "--bottom", help="Send to the back of the ranked band in its lane"
    ),
    before: Optional[str] = typer.Option(
        None, "--before", help="Place just before a ranked anchor in the same lane"
    ),
    after: Optional[str] = typer.Option(
        None, "--after", help="Place just after a ranked anchor in the same lane"
    ),
    clear: bool = typer.Option(
        False, "--clear", help="Clear the rank (rejoin the unranked priority flow)"
    ),
    within_epic: bool = typer.Option(
        False,
        "--within-epic",
        help="Rank within the node's live epic (child default; refused without one)",
    ),
) -> None:
    """Curate a node's position within its (column, project) board lane.

    Rank is a nullable float ordered ahead of the shared epic-aware work-order
    suffix within a lane; it never changes a node's column. ``--before`` /
    ``--after`` require a *ranked* anchor in the same lane - seed one with
    ``--top`` first. Float midpoints mean inserts never renumber siblings.
    A node with a live epic parent ranks WITHIN that epic (peers and anchor
    are its live-epic siblings, whole graph): the child's rank orders it only
    among its siblings and never moves its epic group. ``--within-epic``
    spells that scope out loud and is refused for a node with no live epic
    parent. Loose nodes and epic containers keep the lane scope.
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph._intake import _find_node, _live_epic_for, _epics_with_child_progress
    from fno.graph.render import _project_key, make_kanban_column
    from fno.graph.store import locked_mutate_graph
    from fno.graph.cli import _graph_path, _project_plans_from_graph

    if not has_node_id_prefix(task_id):
        typer.echo(
            f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True
        )
        raise typer.Exit(code=1)

    chosen = [
        name
        for name, on in (
            ("--top", top),
            ("--bottom", bottom),
            ("--before", before is not None),
            ("--after", after is not None),
            ("--clear", clear),
        )
        if on
    ]
    if len(chosen) != 1:
        typer.echo(
            "Error: pass exactly one of --top / --bottom / --before <id> / --after <id> / --clear",
            err=True,
        )
        raise typer.Exit(code=1)

    anchor_id = before if before is not None else after
    if anchor_id is not None and not has_node_id_prefix(anchor_id):
        typer.echo(
            f"Error: anchor must be a <prefix>-<4..8 hex> node id, got '{anchor_id}'", err=True
        )
        raise typer.Exit(code=1)

    result: dict = {}

    def _is_ranked(e: dict) -> bool:
        # Match render._rank_band: a non-finite OR huge-int rank (from a
        # hand-edited graph.json) is treated as unranked, so a poisoned peer
        # can't corrupt the --top/--bottom/midpoint arithmetic or persist a
        # NaN/inf rank. float() guards the OverflowError a giant int raises.
        r = e.get("rank")
        if isinstance(r, bool) or not isinstance(r, (int, float)):
            return False
        try:
            return math.isfinite(float(r))
        except (OverflowError, ValueError):
            return False

    def mutator(entries):
        try:
            column_for = make_kanban_column(entries, strict_claims=True)
        except Exception as exc:
            typer.echo(
                "Error: live claim state is unavailable; rank refused without changing the graph.",
                err=True,
            )
            raise typer.Exit(code=1) from exc

        def _lane(e: dict) -> tuple:
            return (column_for(e), _project_key(e))

        def _lane_label(e: dict) -> str:
            col, proj = _lane(e)
            return f"{col or '(off-board)'}/{proj}"

        node = _find_node(entries, task_id)
        if not node:
            typer.echo(f"Error: feature {task_id} not found", err=True)
            raise typer.Exit(code=1)
        # _find_node fuzzy-resolves partial ids (e.g. `ab-9728`); compare on
        # the RESOLVED id everywhere below so the target is excluded from its
        # own peer set and the self-anchor guard fires for partial input.
        tid = node.get("id") or task_id

        # Whole-graph live-epic scope, resolved by the SAME helper the
        # selection key uses, so the peers a rank orders among are exactly
        # the siblings the work order compares it against.
        id_to_entry = {
            e["id"]: e
            for e in entries
            if isinstance(e, dict) and isinstance(e.get("id"), str)
        }
        child_progress = _epics_with_child_progress(id_to_entry)

        def _epic_of(e: object) -> str | None:
            parent = _live_epic_for(e, id_to_entry, child_progress)
            return parent["id"] if parent is not None else None

        epic_id = _epic_of(node)
        if within_epic and epic_id is None:
            typer.echo(
                f"Error: --within-epic refused: {tid} has no live epic parent. "
                "Child ranking needs a live epic; loose nodes and epic "
                "containers keep the (column, project) lane scope.",
                err=True,
            )
            raise typer.Exit(code=1)

        if clear:
            node["rank"] = None
            result.update(action="--clear", rank=None, lane=_lane_label(node), id=tid)
            return entries

        if epic_id is not None:
            # Child scope: the whole graph's children of the same live epic.
            # The child's rank orders only within its epic group, so peers
            # and anchors come from that set, not the board lane.
            scope_label = f"epic {epic_id}"
            peers = [
                e for e in entries
                if isinstance(e, dict) and e.get("id") != tid and _epic_of(e) == epic_id
            ]
        else:
            scope_label = _lane_label(node)
            target_lane = _lane(node)
            peers = [
                e for e in entries
                if isinstance(e, dict) and e.get("id") != tid and _lane(e) == target_lane
            ]
        # Peers exclude the target; ranked peers (anchor included) sorted
        # ascending give us the band to insert into.
        ranked = sorted((e for e in peers if _is_ranked(e)), key=lambda e: e["rank"])

        if top:
            new_rank = (ranked[0]["rank"] - 1.0) if ranked else 0.0
            action = "--top"
        elif bottom:
            new_rank = (ranked[-1]["rank"] + 1.0) if ranked else 0.0
            action = "--bottom"
        else:
            anchor = _find_node(entries, anchor_id)
            if not anchor:
                typer.echo(f"Error: anchor {anchor_id} not found", err=True)
                raise typer.Exit(code=1)
            if anchor.get("id") == tid:
                typer.echo("Error: cannot rank a node relative to itself", err=True)
                raise typer.Exit(code=1)
            if epic_id is not None:
                anchor_epic = _epic_of(anchor)
                if anchor_epic != epic_id:
                    typer.echo(
                        f"Error: cross-epic rank rejected: {task_id} is a child of "
                        f"{epic_id} but anchor {anchor_id} is "
                        f"{'loose' if anchor_epic is None else f'a child of {anchor_epic}'}. "
                        "Child rank is scoped to its live epic.",
                        err=True,
                    )
                    raise typer.Exit(code=1)
            elif _lane(anchor) != target_lane:
                typer.echo(
                    f"Error: cross-lane rank rejected: {task_id} is in "
                    f"{_lane_label(node)} but anchor {anchor_id} is in "
                    f"{_lane_label(anchor)}. Rank is scoped per (column, project) lane.",
                    err=True,
                )
                raise typer.Exit(code=1)
            if not _is_ranked(anchor):
                typer.echo(
                    f"Error: anchor {anchor_id} is unranked; rank it first "
                    f"(e.g. `fno backlog rank {anchor_id} --top`) or use --top/--bottom.",
                    err=True,
                )
                raise typer.Exit(code=1)
            anchor_rank = float(anchor["rank"])
            if before is not None:
                lowers = [e["rank"] for e in ranked if e["rank"] < anchor_rank]
                lo = max(lowers) if lowers else None
                new_rank = (anchor_rank - 1.0) if lo is None else (lo + anchor_rank) / 2.0
                action = f"--before {anchor_id}"
            else:
                highers = [e["rank"] for e in ranked if e["rank"] > anchor_rank]
                hi = min(highers) if highers else None
                new_rank = (anchor_rank + 1.0) if hi is None else (anchor_rank + hi) / 2.0
                action = f"--after {anchor_id}"

        node["rank"] = new_rank
        result.update(action=action, rank=new_rank, lane=scope_label, id=tid)
        return entries

    graph_path = _graph_path()
    locked_mutate_graph(graph_path, mutator)
    if result.get("action") == "--clear":
        typer.echo(
            f"Cleared rank on {result['id']} (rejoined the unranked flow in {result['lane']})"
        )
    else:
        note = _dispatch_note(result["id"], graph_path)
        suffix = f"; {note}" if note else ""
        typer.echo(
            f"Ranked {result['id']} {result['action']} (rank={result['rank']}) in "
            f"{result['lane']}{suffix}"
        )
    _project_plans_from_graph([result["id"]])
