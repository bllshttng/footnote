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
import os
from pathlib import Path
from typing import Optional

import typer

from fno.agents.mux_spawn import dispatch_spawn_pane, resolve_provenance
from fno.backlog.advance import (
    _DISPATCH_TTL_MS,
    _claim_is_live,
    _claims_root_for,
    _next_node,
    _worker_agent_name,
)
from fno.claims import ClaimHeldByOther, acquire_claim, release_claim
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
    account: Optional[str] = typer.Option(
        None,
        "--account",
        help="Pin the spawned worker to a registered claude account (x-d012 "
        "overlay); the mux passes its session-local active account here.",
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
    verdict = _dispatch_one(session=session, node=node, project=project, account=account)
    if json_output:
        typer.echo(json.dumps(verdict))
    else:
        line = verdict["outcome"]
        if verdict.get("node"):
            line += f" {verdict['node']}"
        typer.echo(line)
    raise typer.Exit(code=0 if verdict["outcome"] != "failed" else 1)


@dispatch_app.command("resolve")
def cmd_resolve(
    harness: Optional[str] = typer.Option(
        None, "--harness", help="Target harness (claude|codex|gemini|agy|opencode). Default: config.dispatch.harness > claude."
    ),
    substrate: Optional[str] = typer.Option(
        None, "--substrate", help="bg|headless|pane. Default: per-harness (claude=bg, else headless)."
    ),
    node: Optional[str] = typer.Option(
        None, "--node", "--id", help="Node id substituted into the command's {id}. Absent = template returned literally."
    ),
    command: Optional[str] = typer.Option(
        None, "--command", help="Command template. Default: config.dispatch.command > '/target no-merge {id}'."
    ),
    verb: Optional[str] = typer.Option(
        None, "--verb", help="Node dispatch verb (validated against config.dispatch.allowed_verbs); assembled as '<verb> {id}'. Wins over --command's config/builtin default."
    ),
    brief: Optional[str] = typer.Option(
        None, "--brief", help="Node dispatch brief; returned in env.TARGET_BRIEF (never the command line). Capped at 8 KB."
    ),
    trigger: str = typer.Option(
        "autonomous", "--trigger", help="autonomous (fire-and-forget) | attended. Autonomous never resolves pane."
    ),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the resolved tuple as JSON (default: key=value lines)."
    ),
) -> None:
    """Resolve (config + context) -> (harness, substrate, command, permission_bypass, env).

    Pure: reads the harness-capability map + config.dispatch, resolves nothing at
    runtime, never spawns or claims. Exit 0 on a resolved tuple; exit 2 naming the
    harness and the map when it cannot resolve (unknown harness, bad substrate,
    empty/unsubstituted command).
    """
    from fno.agents.harness_map import DispatchResolveError, resolve_dispatch

    try:
        out = resolve_dispatch(
            harness=harness,
            substrate=substrate,
            node_id=node,
            command=command,
            verb=verb,
            brief=brief,
            trigger=trigger,
        )
    except DispatchResolveError as exc:
        typer.echo(f"dispatch resolve: {exc}", err=True)
        raise typer.Exit(code=2)

    if json_output:
        typer.echo(json.dumps(out))
    else:
        for key in ("harness", "substrate", "command", "command_surface"):
            typer.echo(f"{key}={out[key]}")
        typer.echo(f"permission_bypass={' '.join(out['permission_bypass'])}")
        typer.echo(f"bg={out['bg']}")
        typer.echo(f"resume={out['resume']}")
        # env carries TARGET_BRIEF (US3); consumers read it via -J JSON. A brief
        # can be multi-line, so key=value lines only report presence/size here.
        if out["env"].get("TARGET_BRIEF") is not None:
            typer.echo(f"brief_bytes={len(out['env']['TARGET_BRIEF'].encode('utf-8'))}")
    raise typer.Exit(code=0)


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


def _resolve_provider_id() -> Optional[str]:
    """The provider record a default dispatch would run on (the active one).

    Best-effort: an unconfigured / unreadable providers block yields None, which
    reads as UNKNOWN headroom and proceeds (fail-open)."""
    try:
        from fno.adapters.providers.loader import load_providers

        return load_providers().active
    except Exception:  # noqa: BLE001 - a config read must never block a dispatch
        return None


def _emit_quota_deferred(node_id: str, provider: str, state: str, retry_at: Optional[float]) -> None:
    """Emit the single quota_deferred decision event. Non-fatal (AC1-UI)."""
    try:
        from fno.events import _build, append_event

        data: dict = {"node_id": node_id, "provider": provider, "headroom": state}
        if retry_at is not None:
            data["retry_at"] = retry_at
        append_event(_build("quota_deferred", "backlog", data))
    except Exception:  # noqa: BLE001 - a telemetry write must never block dispatch
        pass


def _dispatch_one(
    *,
    session: str,
    node: Optional[str],
    project: Optional[str],
    account: Optional[str] = None,
) -> dict:
    # 0. Resolve the account overlay CLI-side (x-d012 owns the resolver + the
    #    stale/missing-account refusal). A bad account fails the verdict here
    #    rather than silently spawning under the wrong (default) account (AC2-ERR).
    account_env: Optional[dict[str, str]] = None
    if account:
        from fno.agents.account_env import (
            AccountResolutionError,
            resolve_account_overlay,
        )

        try:
            account_env = resolve_account_overlay(account).env
        except AccountResolutionError as exc:
            return {"outcome": "failed", "detail": f"--account {account}: {str(exc)[:180]}"}

    # 1. Select the node: explicit --node, else the board's next ready one.
    if node:
        rec = _lookup_node(node)
        node_id = rec.get("id") if rec else node
        slug = rec.get("slug") if rec else None
        cwd = (rec.get("_resolved_cwd") or rec.get("cwd")) if rec else None
        priority = rec.get("priority") if rec else None
        explicit = True  # explicit --node is a human verb; never quota-defers (LD#5)
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
        priority = picked.get("priority")
        explicit = False

    if not isinstance(node_id, str) or not node_id:
        return {"outcome": "failed", "detail": "resolved node has no id"}

    # 1b. Quota-aware defer (x-5d3e). Only the ambient/autonomous default
    #     selection defers; an explicit --node dispatch always fires (LD#5).
    #     Fail-open: defer_dispatch off, p0, or UNKNOWN headroom -> proceed.
    if not explicit:
        from fno.adapters.providers.runtime_state import evaluate_quota_defer

        decision = evaluate_quota_defer(_resolve_provider_id() or "", priority=priority)
        if decision is not None:
            _emit_quota_deferred(node_id, decision.provider_id, decision.state.value, decision.retry_at)
            return {
                "outcome": "quota-deferred",
                "node": node_id,
                "slug": slug or "",
                "provider": decision.provider_id,
                "headroom": decision.state.value,
                "retry_at": decision.retry_at,
            }

    # 2. Boot-window dedup (mirrors advance()): a node already being worked
    #    (live node:<id>) or already mid-dispatch (live dispatch:<id>) is NOT
    #    re-dispatched. The create-only dispatch:<id> reservation is what closes
    #    the same-node race: two fast leader+g both resolve _next_node to the same
    #    node before the first worker claims it; without this reservation both
    #    would share ONE (idempotent) lane slot and the loser's spawn-failure
    #    would free the winner's live slot, defeating the cap. Only the winner of
    #    the O_EXCL reservation proceeds; the loser reports already-dispatching.
    if _claim_is_live(f"node:{node_id}") or _claim_is_live(f"dispatch:{node_id}"):
        return {"outcome": "already-dispatching", "node": node_id, "slug": slug or ""}
    dispatch_key = f"dispatch:{node_id}"
    dispatch_holder = f"dispatch-one:{os.getpid()}"
    dispatch_root = _claims_root_for(dispatch_key)
    try:
        acquire_claim(
            dispatch_key, dispatch_holder,
            ttl_ms=_DISPATCH_TTL_MS, reason=f"mux dispatch for {node_id}", root=dispatch_root,
        )
    except ClaimHeldByOther:
        return {"outcome": "already-dispatching", "node": node_id, "slug": slug or ""}

    # 3. Atomic lane cap (config.parallel.max_lanes). A full cap -> lanes-full:
    #    no lane, no spawn (AC-edge). max_lanes 0 would forbid every manual grab,
    #    so a deliberate keystroke floors it to one slot. Free the reservation on
    #    a full cap so the node stays re-dispatchable.
    max_lanes = max(1, load_settings().parallel.max_lanes or 1)
    slot = acquire_lane_slot(max_lanes, node_id)
    if slot is None:
        release_claim(dispatch_key, dispatch_holder, root=dispatch_root)
        return {"outcome": "lanes-full", "node": node_id, "slug": slug or ""}

    # 4. Spawn the pane worker into THIS session. Any failure releases BOTH the
    #    lane slot and the dispatch reservation so the node stays re-dispatchable
    #    - never a phantom lane holding the cap. On success dispatch:<id> is left
    #    to TTL-expire (bridges the boot window until the worker owns node:<id>).
    workdir = Path(cwd) if cwd else Path.cwd()
    # (x-c914) Stamp the birth account into the pane provenance (FNO_ACCOUNT)
    # when routed, so the mux server reads it back for the sideline account
    # glyph - a managed account shares ~/.claude, so the roster can't
    # distinguish it, but the pane's own env can (Locked Decision 5: pane env,
    # not the registry schema).
    provenance = resolve_provenance(node_id, slug)
    if account:
        provenance["FNO_ACCOUNT"] = account
    try:
        result = dispatch_spawn_pane(
            name=_worker_agent_name(node_id, slug),
            message=f"/target no-merge {node_id}",
            provider="claude",
            cwd=workdir,
            session=session,
            provenance=provenance,
            account_env=account_env,
        )
    except Exception as exc:  # noqa: BLE001 - DispatchAskError or any spawn error
        release_lane_slot(node_id)
        release_claim(dispatch_key, dispatch_holder, root=dispatch_root)
        return {"outcome": "failed", "node": node_id, "slug": slug or "", "detail": str(exc)[:200]}
    return {
        "outcome": "launched",
        "node": node_id,
        "slug": slug or "",
        "pane_id": result.pane_id,
    }
