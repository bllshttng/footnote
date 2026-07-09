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
        help="Live transcript session id (default: ambient $CODEX_THREAD_ID / "
             "$CLAUDE_CODE_SESSION_ID / $CODEX_SESSION_ID / $GEMINI_SESSION_ID).",
    ),
    cwd: Optional[str] = typer.Option(
        None, "--cwd", help="Live session cwd (default: the current working dir)."
    ),
    harness: Optional[str] = typer.Option(
        None, "--harness",
        help="Live session harness (default: ambiently detected; claude/codex/gemini).",
    ),
    model: Optional[str] = typer.Option(
        None, "--model", "-m",
        help="Model to pass to the spawned worker's provider CLI (exact passthrough).",
    ),
    provider: Optional[str] = typer.Option(
        None, "--provider", "-p",
        help="Provider for the spawned worker (default: claude; detached /think "
             "refuses non-claude providers because its bg substrate is Claude-only).",
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
    from fno.agents.provider_resolve import (
        DispatchFlagError,
        reject_empty_model,
        resolve_dispatch_provider,
    )
    from fno.graph.cli import _graph_path, _session_provenance
    from fno.graph.fuzzy import resolve_node
    from fno.graph.store import read_graph
    from fno.provenance.spawn_think import dispatch_conversational

    # Validate the dispatch flags up front (empty --model/--provider is a usage
    # error, not a silently-forwarded empty argv token). Provider is resolved only
    # when given: absent, the bg /think substrate keeps its claude default rather
    # than inferring the invoking harness (which bg cannot host anyway).
    try:
        model = reject_empty_model(model)
        resolved_provider = (
            resolve_dispatch_provider(provider)[0] if provider is not None else None
        )
    except DispatchFlagError as exc:
        typer.echo(f"fno think dispatch: {exc}", err=True)
        raise typer.Exit(code=2)

    # Resolve the LIVE session pointer ambiently across all three harnesses (the
    # same capture node-birth provenance uses, x-30f6), so the verb works in a
    # codex/gemini session too, not only claude (codex P2). Explicit flags win.
    prov = _session_provenance()
    sid = (session_id or prov.get("source_session_id") or "").strip()
    live_harness = (harness or prov.get("source_harness") or "claude").strip()
    if resolved_provider is not None and resolved_provider != "claude":
        if live_harness == "codex":
            typer.echo("codex posture: think source=codex; dispatch=unsupported")
        typer.echo(
            "fno think dispatch: detached /think uses Claude bg; omit --provider "
            "to use the Claude fallback; non-Claude headless is a one-shot with "
            "no live think-session receipt and is not supported by this verb.",
            err=True,
        )
        raise typer.Exit(code=2)
    if not sid:
        typer.echo(
            "fno think dispatch: no live session id - set --session-id or run "
            "inside a claude/codex/gemini session ($CODEX_THREAD_ID / "
            "$CLAUDE_CODE_SESSION_ID / $CODEX_SESSION_ID / $GEMINI_SESSION_ID). "
            "There is nothing to carry "
            "without a live pointer.",
            err=True,
        )
        raise typer.Exit(code=2)
    # Normalize an explicit --cwd to an absolute path; otherwise use the session
    # cwd (gemini: don't pass a relative/whitespace path to the dispatch core).
    live_cwd = (os.path.abspath(cwd.strip()) if cwd and cwd.strip()
                else (prov.get("source_cwd") or os.getcwd()))

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

    # Overlay the dispatch-time model/provider onto the node. dispatch_conversational
    # spreads the node into its live overlay, and maybe_spawn_think reads
    # node["model"]/node["provider"] at the spawn seam, so a plain key overlay
    # carries the flags all the way to `fno agents spawn` without threading a
    # parameter through every layer.
    if model is not None:
        target["model"] = model
    if resolved_provider is not None:
        target["provider"] = resolved_provider

    if live_harness == "codex":
        typer.echo("codex posture: think source=codex; dispatch=claude-bg-fallback")

    result = dispatch_conversational(
        target, session_id=sid, cwd=live_cwd, harness=live_harness,
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
            "detail": result.detail,
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
