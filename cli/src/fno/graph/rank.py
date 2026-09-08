"""Rank a graph node and qualify whether a live mission can reach it."""

from __future__ import annotations

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
        if any(task_id in descendants_of(entries, m) for m in missions):
            return None
        # The remedy, not just the diagnosis: name the one command that makes a
        # dispatcher take the node. With no epic parent it says so, so the note
        # never prints a command that cannot work.
        me = next((e for e in entries if e.get("id") == task_id), None)
        parent = (me or {}).get("parent")
        if parent:
            remedy = f"; Activate its epic: fno backlog advance --epic {parent}"
        else:
            remedy = ("; no epic to activate (missions are activated per epic "
                      "with fno backlog advance --epic <epic-id>)")
        scope = (
            f"outside active mission scopes: {', '.join(missions)}"
            if missions else "no resolved active missions"
        )
        return f"no live dispatcher will take it ({scope}){remedy}"
    except Exception as exc:  # noqa: BLE001 - rank already committed; qualify unknowns
        return f"dispatcher scope unavailable ({exc})"


def agent_harness_writing_rank(env=None) -> str | None:
    """The harness name when an agent runs this, ``None`` in an operator shell.

    Two provers, either enough. Ancestry catches a session fno never spawned;
    the stamp catches a codex thread worker, which owns no process to walk. A
    half stamp reads as an agent, and an unreadable ancestry fails CLOSED: the
    stamp cannot see what the prover was added for, so falling through to it
    would open the fence exactly when the stronger check broke.
    """
    from fno.harness_identity import parse_canonical_identity

    try:
        from fno.claims.self_identity import resolve_self_identity

        if (owned := resolve_self_identity(env).harness):
            return owned
    except Exception as exc:  # noqa: BLE001 - fail closed, and name why
        typer.echo(f"note: harness ancestry unreadable ({exc}); refusing", err=True)
        return "unprovable"
    identity = parse_canonical_identity(env)
    return None if identity.disposition == "absent" else (identity.harness or "agent")


def _agent_rank_refusal(task_id: str, harness: str) -> str:
    return (
        f"Error: rank is operator-only; this {harness} session may not write it.\n"
        "Every --top writes min(rank) - 1, so agent pins form a stack in which "
        "the last writer wins and importance is never computed.\nVote instead:\n"
        f"  fno backlog encounter {task_id} --evidence \"what it cost you\"\n"
        f"  fno backlog update {task_id} --priority p1|p2|p3\n"
        "The graph records no writer for a rank, so nothing downstream could "
        "tell yours from the operator's. The escape hatch in --help is theirs."
    )


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
    operator: bool = typer.Option(False, "--operator", help="The operator's own pin, from inside an agent session"),
) -> None:
    """Curate a node's position within its (column, project) board lane.

    Operator-only: a pin every agent can write is a stack. ``--operator`` is
    the escape hatch from inside an agent shell.

    Rank is a nullable float ordered ahead of the shared epic-aware work-order
    suffix within a lane; it never changes a node's column. ``--before`` /
    ``--after`` require a *ranked* anchor in the same lane - seed one with
    ``--top`` first. Float midpoints mean inserts never renumber siblings.
    A node with a live epic parent ranks WITHIN that epic (peers and anchor are
    its live-epic siblings, whole graph): the child's rank orders it only among
    its siblings and never moves its epic group. ``--within-epic`` spells that
    scope out loud and is refused without a live epic parent. Loose nodes and
    epic containers keep the lane scope.
    """
    from fno.graph._constants import has_node_id_prefix, _rank_band
    from fno.graph._intake import _find_node, _live_epic_for, _epics_with_child_progress
    from fno.graph.render import _project_key, make_kanban_column
    from fno.graph.store import locked_mutate_graph
    from fno.graph.cli import _graph_path, _project_plans_from_graph

    if not has_node_id_prefix(task_id):
        typer.echo(
            f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True
        )
        raise typer.Exit(code=1)

    if not operator:
        harness = agent_harness_writing_rank()
        if harness is not None:
            typer.echo(_agent_rank_refusal(task_id, harness), err=True)
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
        # The board's own definition, not a second copy of it: a poisoned peer
        # degrades to unranked there, so it cannot corrupt the arithmetic here.
        return _rank_band(e)[0] == 0

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
            scope_kind = "epic"
            peers = [
                e for e in entries
                if isinstance(e, dict) and e.get("id") != tid and _epic_of(e) == epic_id
            ]
        else:
            scope_label = f"lane {_lane_label(node)}"
            scope_kind = "lane"
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
        result.update(
            action=action, rank=new_rank, lane=scope_label, scope_kind=scope_kind, id=tid
        )
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
        # A bare "Ranked --top" read as "runs next across the project" and
        # meant "top of its own epic". Name what the pin is top OF.
        scope_note = (
            "orders it among that epic's children only, and the epic's own "
            "rank decides where the group runs"
            if result.get("scope_kind") == "epic"
            else "orders it within that board lane only"
        )
        typer.echo(
            f"Ranked {result['id']} {result['action']} of {result['lane']} "
            f"(rank={result['rank']}); {scope_note}{suffix}"
        )
    _project_plans_from_graph([result["id"]])
