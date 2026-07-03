"""fno dispatch: grab one ready node into a mux pane (x-6f77).

The mux's ``leader+g`` ("grab work") shells here. One verb, one JSON verdict,
so the Rust caller renders a notice from a single exec instead of stitching
``fno backlog next`` + spawn + cap checks itself. Pure composition of shipped
machinery:

- selection: ``advance._next_node`` (the same board order ``fno backlog next`` uses)
- concurrency cap: the atomic ``acquire_lane_slot`` over ``config.parallel.max_lanes``
- spawn: ``dispatch_spawn_pane`` (pane substrate, into THIS session)

Never double-claims: the lane slot is the concurrency authority, and the spawned
worker's own ``fno target start`` claims ``node:<id>`` and re-anchors the slot to
its lifecycle (target_cli._maybe_reconcile_lane_slot) - identical to the daemon
``dispatch-lanes`` path, so the slot frees when the worker ends.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Optional

import typer

from fno.agents.mux_spawn import dispatch_spawn_pane, resolve_provenance
from fno.backlog.advance import _next_node, _worker_agent_name
from fno.claims.lanes import acquire_lane_slot, release_lane_slot
from fno.config import load_settings

dispatch_app = typer.Typer(
    no_args_is_help=True, help="Dispatch ready work into mux panes."
)


@dispatch_app.callback()
def _dispatch_callback() -> None:
    """No-op: keeps Typer from collapsing the single-command sub-app (a one-@command
    app otherwise swallows the ``one`` subcommand name)."""


@dispatch_app.command("one")
def cmd_one(
    session: str = typer.Option(
        ..., "--mux-session", help="Mux session to spawn the pane into (FNO_SESSION)."
    ),
    node: Optional[str] = typer.Option(
        None, "--node", help="Dispatch this node id/slug (default: fno backlog next)."
    ),
    project: Optional[str] = typer.Option(
        None, "--project", "-p", help="Scope the default selection to a project."
    ),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit a one-line JSON verdict."
    ),
) -> None:
    """Dispatch one ready node into a new pane in SESSION, respecting the lane cap.

    Verdict ``outcome`` is one of ``launched | no-work | lanes-full | failed``.
    Exit 0 for the first three (a full cap / empty backlog is not an error the
    caller retries); exit 1 for ``failed``.
    """
    verdict = _dispatch_one(session=session, node=node, project=project)
    if json_output:
        typer.echo(json.dumps(verdict))
    else:
        line = verdict["outcome"]
        if verdict.get("node"):
            line += f" {verdict['node']}"
        typer.echo(line)
    raise typer.Exit(code=0 if verdict["outcome"] != "failed" else 1)


def _lookup_node(node_ref: str) -> Optional[dict]:
    """Best-effort graph record for an explicit ``--node`` (id or slug). A
    missing/corrupt graph degrades to None; the dispatch still proceeds with the
    raw id and cwd falls back to the launch dir."""
    try:
        from fno.graph.load import load_graph

        for rec in load_graph():
            if rec.get("id") == node_ref or rec.get("slug") == node_ref:
                return rec
    except Exception:  # noqa: BLE001 - a graph read must never block a dispatch
        return None
    return None


def _dispatch_one(
    *, session: str, node: Optional[str], project: Optional[str]
) -> dict:
    # 1. Select the node: explicit --node, else the board's next ready one.
    if node:
        rec = _lookup_node(node)
        node_id = rec.get("id") if rec else node
        slug = rec.get("slug") if rec else None
        cwd = (rec.get("_resolved_cwd") or rec.get("cwd")) if rec else None
    else:
        try:
            picked = _next_node(project)
        except RuntimeError as exc:  # garbled `fno backlog next` - skip, don't guess
            return {"outcome": "failed", "detail": str(exc)[:200]}
        if not picked:
            return {"outcome": "no-work"}
        node_id = picked["id"]
        slug = picked.get("slug")
        cwd = picked.get("_resolved_cwd") or picked.get("cwd")

    # 2. Atomic lane cap (config.parallel.max_lanes). A full cap -> lanes-full:
    #    no claim, no spawn (AC-edge). max_lanes 0 would forbid every manual grab,
    #    so a deliberate keystroke floors it to one slot.
    max_lanes = max(1, load_settings().config.parallel.max_lanes or 1)
    slot = acquire_lane_slot(max_lanes, node_id)
    if slot is None:
        return {"outcome": "lanes-full", "node": node_id, "slug": slug or ""}

    # 3. Spawn the pane worker into THIS session. Any failure releases the slot so
    #    the node stays re-dispatchable - never a phantom lane holding the cap.
    workdir = Path(cwd) if cwd else Path.cwd()
    try:
        result = dispatch_spawn_pane(
            name=_worker_agent_name(node_id, slug),
            message=f"/target no-merge {node_id}",
            provider="claude",
            cwd=workdir,
            session=session,
            provenance=resolve_provenance(node_id, slug),
        )
    except Exception as exc:  # noqa: BLE001 - DispatchAskError or any spawn error
        release_lane_slot(node_id)
        return {"outcome": "failed", "node": node_id, "slug": slug or "", "detail": str(exc)[:200]}
    return {
        "outcome": "launched",
        "node": node_id,
        "slug": slug or "",
        "pane_id": result.pane_id,
    }
