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
        for target in resolve_drain_targets():
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
        if missions:
            return (
                "no live dispatcher will take it "
                f"(outside active mission scopes: {', '.join(missions)})"
            )
        return "no live dispatcher will take it (no resolved active missions)"
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
) -> None:
    """Curate a node's position within its (column, project) board lane.

    Rank is a nullable float ordered ahead of the shared epic-aware work-order
    suffix within a lane; it never changes a node's column. ``--before`` /
    ``--after`` require a *ranked* anchor in the same lane - seed one with
    ``--top`` first. Float midpoints mean inserts never renumber siblings.
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph._intake import _find_node
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

        if clear:
            node["rank"] = None
            result.update(action="--clear", rank=None, lane=_lane_label(node), id=tid)
            return entries

        target_lane = _lane(node)
        # Lane peers exclude the target; ranked peers (anchor included) sorted
        # ascending give us the band to insert into.
        peers = [e for e in entries if e.get("id") != tid and _lane(e) == target_lane]
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
            if _lane(anchor) != target_lane:
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
        result.update(action=action, rank=new_rank, lane=_lane_label(node), id=tid)
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
