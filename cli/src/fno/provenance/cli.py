"""CLI surface for the explicit conversational /think dispatch verb (x-0a9c, Wave C).

``fno think dispatch <node>`` - the operator, mid-conversation about an
fno-touched node, hands it to a bg /think that picks it up with full LIVE
context. The ``/think`` skill's ``dispatch`` mode is the front door; this verb is
the deterministic mechanism (Locked Decision 6: route through
:func:`fno.provenance.spawn_think.maybe_spawn_think`, never hand-assemble a
spawn). Explicit-only - there is no auto-grep detector (AC5-FR).
"""
from __future__ import annotations

import json
import os
from typing import Optional

import typer

think_app = typer.Typer(
    name="think",
    help="Context /think dispatch (explicit conversational verb, x-0a9c).",
    no_args_is_help=True,
    add_completion=False,
)


@think_app.callback()
def _callback() -> None:
    """Keep ``dispatch`` a real subcommand.

    A Typer app with a single command auto-collapses it into the main callback,
    which would make ``fno think dispatch <node>`` parse ``dispatch`` as NODE.
    An explicit (no-op) callback preserves the ``fno think dispatch`` shape.
    """


@think_app.command("dispatch")
def dispatch(
    node: str = typer.Argument(
        ..., help="Backlog node id / slug / bare-hex to dispatch a /think for."
    ),
    session_id: Optional[str] = typer.Option(
        None, "--session-id",
        help="Live transcript session id (default: $CLAUDE_CODE_SESSION_ID).",
    ),
    cwd: Optional[str] = typer.Option(
        None, "--cwd", help="Live session cwd (default: the current working dir)."
    ),
    harness: str = typer.Option(
        "claude", "--harness", help="Live session harness (claude/codex/gemini)."
    ),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the dispatch result as JSON."
    ),
) -> None:
    """Dispatch a bg /think for NODE carrying THIS session's live transcript pointer.

    Resolves NODE in the graph, overlays the live ``(harness, session_id, cwd)``
    pointer, and routes through the shared dispatch core. Exit codes: 0 spawned/
    offered, 1 skipped (e.g. dedup / daily-cap), 2 bad input (no live session id
    or node not found).
    """
    from fno.graph.cli import _graph_path
    from fno.graph.fuzzy import resolve_node
    from fno.graph.store import read_graph
    from fno.provenance.spawn_think import dispatch_conversational

    sid = (session_id or os.environ.get("CLAUDE_CODE_SESSION_ID") or "").strip()
    if not sid:
        typer.echo(
            "fno think dispatch: no live session id - set --session-id or run "
            "inside a claude session ($CLAUDE_CODE_SESSION_ID). There is nothing "
            "to carry without a live pointer.",
            err=True,
        )
        raise typer.Exit(code=2)
    live_cwd = cwd or os.getcwd()

    # Deterministic resolution tiers 1-3 (exact id / slug / bare-hex) - the same
    # resolver `fno backlog get` uses, so every exact entry form resolves.
    match = resolve_node(node, read_graph(_graph_path()))
    if match.kind != "exact":
        typer.echo(
            f"fno think dispatch: no node matches {node!r} (id/slug/bare-hex).",
            err=True,
        )
        raise typer.Exit(code=2)

    target = match.candidates[0]
    # Resolve the worker cwd to the node's project root (work-map), like `get`,
    # so the spawned /think runs in the node's repo - not whatever stale cwd the
    # node carries. Falls back to the recorded cwd when the project is unmapped.
    from fno.graph._intake import project_root_from_settings

    proj = target.get("project")
    root = project_root_from_settings(proj) if proj else None
    if root:
        target["_resolved_cwd"] = root

    result = dispatch_conversational(
        target, session_id=sid, cwd=live_cwd, harness=harness,
    )

    if json_output:
        typer.echo(json.dumps({
            "decision": result.decision,
            "event": result.event,
            "reason": result.reason,
            "node_id": result.node_id,
            "presence": result.presence,
            "resolved": result.resolved,
            "think_session": result.think_session,
        }))
    elif result.decision == "spawned":
        typer.echo(
            f"think dispatched: {result.node_id} -> bg /think "
            f"{result.think_session} (live pointer resolved={result.resolved}). "
            f"Watch it with: fno agents watch"
        )
    else:
        typer.echo(
            f"think dispatch {result.decision}: {result.node_id} "
            f"({result.reason})"
        )

    if result.decision == "skipped":
        raise typer.Exit(code=1)
