"""fno agents dispatch: grab one ready node into a mux pane (x-6f77).

The mux's ``leader+g`` ("grab work") shells here. One verb, one JSON verdict,
so the Rust caller renders a notice from a single exec instead of stitching
``fno backlog next`` + spawn + cap checks itself. Pure composition of shipped
machinery:

- selection: ``advance._next_node`` (the same board order ``fno backlog next`` uses)
- admission: the SHARED family-2 guard (``_spawn_guard_decision``) plus the
  spawn gate (``run_gate`` over ``agents.max_live`` / ``min_free_gb`` /
  ``max_load_per_cpu``), the same gate every ``fno agents spawn`` passes -
  exactly one fleet ceiling, not two disjoint caps each blind to the other's
  workers (x-3f84 W5)
- spawn: ``dispatch_spawn_pane`` (pane substrate, into THIS session)

Never double-claims: the guard takes ``dispatch:<id>`` and the handover
``node:<id>`` claim, and the spawned worker's own ``fno do target start``
re-anchors the node claim to its lifecycle.
"""

from __future__ import annotations

import dataclasses
import json
import os
import sys
from pathlib import Path
from typing import Optional

import typer

from fno.agents.mux_spawn import dispatch_spawn_pane, resolve_provenance
from fno.backlog.advance import (
    _next_node,
    _worker_agent_name,
)

dispatch_app = typer.Typer(no_args_is_help=True, help="Dispatch ready work into mux panes.")


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
    """Dispatch one ready node into a new pane in SESSION, through the spawn gate.

    Verdict ``outcome`` is one of ``launched | no-work | already-dispatching |
    quota-deferred | failed`` (plus the guard's own refusal reasons). A full
    fleet no longer returns a verdict: the spawn gate queues inside ``run_gate``
    or refuses with its own exit code. Exit 0 for everything but ``failed``.
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
        None,
        "--substrate",
        help="bg|headless|pane. Default: per-harness (claude + opencode=bg, else headless).",
    ),
    node: Optional[str] = typer.Option(
        None, "--node", "--id", help="Node id substituted into the command's {id}. Absent = template returned literally."
    ),
    command: Optional[str] = typer.Option(
        None, "--command", help="Command template. Default: config.dispatch.command > '/target --no-merge {id}'."
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
    autonomous: bool = typer.Option(
        False,
        "--autonomous",
        help=(
            "Fold the shared quota route decision into the tuple, so a shell "
            "dispatcher gets the same stay/defer/cutover verdict the Python "
            "launchers get. Adds route_action, route_reason, route_account, "
            "route_source, route_window; on a cutover, harness and command are "
            "already the destination's. Off by default: the bare verb stays pure."
        ),
    ),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the resolved tuple as JSON (default: key=value lines)."
    ),
) -> None:
    """Resolve (config + context) -> (harness, substrate, command, permission_bypass, env).

    Pure by default: reads the harness-capability map + config.dispatch, resolves
    nothing at runtime, never spawns or claims. ``--autonomous`` adds ONE runtime
    read - the shared quota route decision - so the shell dispatchers converge on
    the same seam as `backlog advance` and `fno agents dispatch` instead of routing
    around it. Exit 0 on a resolved tuple; exit 2 naming the harness and the map
    when it cannot resolve (unknown harness, bad substrate, empty/unsubstituted
    command).
    """
    from fno.agents.harness_map import DispatchResolveError, resolve_dispatch

    # Auto-brief (x-d1f4): with --node but no explicit --brief, resolve the node's
    # brief chain (explicit dispatch_brief > sidecar > details > transcript tail)
    # so EVERY dispatcher routing through this porcelain - the /target bg shell
    # dispatcher (dispatch-node.sh) included - carries the same context, not only
    # advance.py's daemon paths. An explicit --brief still wins (it IS rung 1).
    brief_source = "explicit" if brief else "none"
    # An explicit --brief is rung 1 and the node is never consulted for it, so
    # the lookup stays demand-driven: only a brief that must be synthesized, or
    # the autonomous route (which reads the node's priority and cwd), pays it.
    rec = _lookup_node(node) if node and (brief is None or autonomous) else None
    if brief is None and rec:
        from fno.provenance.autobrief import resolve_dispatch_brief

        brief, brief_source = resolve_dispatch_brief(rec)

    route = _autonomous_route_for(rec, harness, node) if autonomous else None
    base_harness = harness
    if route is not None and route.action == "cutover":
        # The destination owns the harness AND the command surface (codex takes
        # `$fno:target`, never a raw slash verb), so resolve the tuple FOR it
        # rather than resolving here and patching the harness afterwards.
        harness = route.harness

    def _resolve(target_harness: Optional[str]) -> dict:
        return resolve_dispatch(
            harness=target_harness,
            substrate=substrate,
            node_id=node,
            command=command,
            verb=verb,
            brief=brief,
            trigger=trigger,
        )

    try:
        out = _resolve(harness)
    except DispatchResolveError as exc:
        # A cutover whose destination cannot render is not a cutover - but the
        # quota verdict behind it still stands, so falling back to the ORIGINAL
        # harness would launch on the very account the selector ruled out. Carry
        # the route's own fallback instead: defer when the window was binding,
        # stay when it was only the proactive LOW case.
        if route is not None and route.action == "cutover":
            route = dataclasses.replace(
                route,
                action="defer" if route.defer_fallback else "stay",
                reason=f"{route.reason}-destination-unrenderable",
                record_id=None,
                harness=None,
                account_env=None,
            )
            try:
                out = _resolve(base_harness)
            except DispatchResolveError as exc2:
                typer.echo(f"dispatch resolve: {exc2}", err=True)
                raise typer.Exit(code=2)
        else:
            typer.echo(f"dispatch resolve: {exc}", err=True)
            raise typer.Exit(code=2)

    out["brief_source"] = brief_source
    if autonomous:
        # Report the record id only; the credentials ride `fno agents spawn
        # --dispatch-account`, never argv.
        out["route_action"] = route.action if route else "unknown-proceed"
        out["route_reason"] = route.reason if route else "route-unavailable"
        out["route_account"] = (route.record_id or "") if route else ""
        out["route_source"] = (route.source_record or "") if route else ""
        out["route_window"] = (route.window or "") if route else ""
        out["route_retry_at"] = (route.retry_at if route else None) or ""
    if json_output:
        typer.echo(json.dumps(out))
    else:
        for key in ("harness", "substrate", "command", "command_surface"):
            typer.echo(f"{key}={out[key]}")
        for key in (
            "route_action",
            "route_reason",
            "route_account",
            "route_source",
            "route_window",
            "route_retry_at",
        ):
            if key in out:
                typer.echo(f"{key}={out[key]}")
        typer.echo(f"permission_bypass={' '.join(out['permission_bypass'])}")
        typer.echo(f"thread={out['thread']}")
        typer.echo(f"resume={out['resume']}")
        # env carries TARGET_BRIEF (US3); consumers read it via -J JSON. A brief
        # can be multi-line, so key=value lines only report presence/size here.
        if out["env"].get("TARGET_BRIEF") is not None:
            typer.echo(f"brief_bytes={len(out['env']['TARGET_BRIEF'].encode('utf-8'))}")
        typer.echo(f"brief_source={brief_source}")
    raise typer.Exit(code=0)


@dispatch_app.command("capabilities")
def cmd_capabilities(
    harness: str = typer.Argument(..., help="Harness to inspect."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit compact JSON."),
) -> None:
    """Print one harness's config-independent capability contract."""
    from fno.agents.harness_map import MAP_VERSION, DispatchResolveError, capabilities

    try:
        out = {"map_version": MAP_VERSION, "harness": harness, **capabilities(harness)}
    except DispatchResolveError as exc:
        typer.echo(f"dispatch capabilities: {exc}", err=True)
        raise typer.Exit(code=2)
    typer.echo(json.dumps(out, separators=(",", ":") if json_output else None, indent=None if json_output else 2))


def _autonomous_route_for(
    rec: Optional[dict], harness: Optional[str], node: Optional[str]
):
    """The shared route decision for a shell dispatcher, or None to proceed.

    An explicit ``--harness`` on the invocation is the strongest pin there is, so
    it never reroutes. Everything else routes through the same
    ``select_autonomous_route`` the Python launchers use, which is the point of
    this rung: the shell path used to skip the quota seam entirely and stayed on
    a walled account while an idle harness sat there.

    Best-effort by design - any failure resolves to None (proceed as configured),
    matching the fail-open stance of every other quota read.
    """
    # A named node we could not resolve has no known repository, and routing it
    # from the CALLER's would pick a combo and an account out of the wrong
    # project's registry. Proceed as configured instead of routing on a guess.
    if node and rec is None:
        return None
    try:
        from fno.agents.autonomous_route import (
            launch_is_pinned,
            select_autonomous_route,
        )

        cwd = (rec or {}).get("_resolved_cwd") or (rec or {}).get("cwd")
        provider_id = _resolve_provider_id(cwd) or ""
        # An explicit harness is a pin, so the launch can only stay or defer -
        # and deferring it on the ACTIVE record's quota would hold a codex launch
        # because a claude account is walled. Those are unrelated pools, so when
        # the pin does not match the probed record's harness there is nothing
        # here worth probing: proceed as configured.
        if (harness or "").strip() and not _record_is_harness(provider_id, harness, cwd):
            return None
        return select_autonomous_route(
            provider_id=provider_id,
            priority=(rec or {}).get("priority"),
            pinned=bool((harness or "").strip())
            or launch_is_pinned(rec, node_cwd=cwd),
            node_cwd=cwd,
            node_id=node or (rec or {}).get("id"),
        )
    except Exception:  # noqa: BLE001 - a quota read must never block a dispatch
        return None


def _record_is_harness(
    provider_id: str, harness: Optional[str], node_cwd: Optional[str]
) -> bool:
    """True when ``provider_id``'s record runs on ``harness``.

    Unknown answers True so the probe still happens: skipping quota policy on a
    read failure is the change that could dispatch onto a walled account, and
    fail-open here means "keep the existing behaviour", not "skip the check".
    """
    try:
        from pathlib import Path as _Path

        from fno.adapters.providers.loader import load_providers

        rec = load_providers(
            repo_root=_Path(node_cwd) if node_cwd else None
        ).by_id.get(provider_id)
        rec_harness = (getattr(rec, "harness", "") or "").strip()
        return not rec_harness or rec_harness == (harness or "").strip()
    except Exception:  # noqa: BLE001 - an unreadable registry keeps today's path
        return True


def _lookup_node(node_ref: str) -> Optional[dict]:
    """Best-effort graph record for an explicit ``--node`` (id or slug). A
    missing/corrupt graph degrades to None; the dispatch still proceeds with the
    raw id and cwd falls back to the launch dir."""
    try:
        from fno.graph.load import load_graph

        for rec in load_graph():
            if rec.get("id") == node_ref or rec.get("slug") == node_ref:
                # A raw graph row carries only the RECORDED `cwd`; the work-map
                # projection that turns a project into a root lives in
                # `fno backlog get`. Without it a project-mapped node would have
                # its quota probed against the dispatcher's own repository.
                if not rec.get("_resolved_cwd") and rec.get("project"):
                    try:
                        from fno.graph._intake import project_root_from_settings

                        root = project_root_from_settings(rec["project"])
                        if root:
                            rec["_resolved_cwd"] = root
                    except Exception:  # noqa: BLE001 - best-effort enrichment
                        pass
                return rec
    except Exception:  # noqa: BLE001 - a graph read must never block a dispatch
        return None
    return None


def _resolve_provider_id(node_cwd: Optional[str] = None) -> Optional[str]:
    """The provider record a default dispatch would run on (the active one).

    Routes through the SAME resolver `fno config accounts list` displays, so a managed
    routing-active pointer the slot has moved past no longer evaluates one
    account's headroom for a worker that spawns on another's credential.

    Scoped to the NODE's repository when one is known: a cross-project dispatch
    that read the active record from the dispatcher's own checkout would judge
    one project's quota for another project's launch.

    Best-effort: an unconfigured / unreadable providers block yields None, which
    reads as UNKNOWN headroom and proceeds (fail-open)."""
    try:
        from fno.adapters.providers.loader import effective_active

        return effective_active(repo_root=Path(node_cwd) if node_cwd else None)
    except Exception:  # noqa: BLE001 - a config read must never block a dispatch
        return None


def _cutover_command(harness: Optional[str], node_id: str) -> str:
    """The destination harness's own target command, or "" if unresolvable.

    This verb hosts a pane in THIS mux session, so the substrate is not the
    destination's default; only the COMMAND needs the per-harness render
    (codex takes `$fno:target`, never a raw slash verb). An empty return is the
    caller's signal to stage nothing - a half-resolved destination must not
    spawn.

    This verb always spawns the no-merge `/target` command, on the normal path
    and on the cutover path alike, so `config.auto_merge.grant` is deliberately
    not consulted here. Going through the full resolver would read it and could
    hand a rerouted worker merge authority that the non-cutover launch never
    gets: quota exhaustion must not change who may merge."""
    try:
        from fno.agents.harness_map import dispatch_command

        return dispatch_command(harness or "", allow_merge=False).replace(
            "{id}", node_id
        )
    except Exception:  # noqa: BLE001 - an unresolvable harness never spawns
        return ""


def _emit_failover(node_id: str, route) -> None:
    """Emit the one cross-harness cutover receipt. Non-fatal, post-spawn only."""
    try:
        from fno.backlog.advance import EVENT_FAILOVER
        from fno.events import _build, append_event

        append_event(
            _build(
                EVENT_FAILOVER,
                "backlog",
                {
                    "node_id": node_id,
                    "from": route.source_record,
                    "to": route.record_id or "",
                    "harness_to": route.harness or "",
                    "window": route.window or "",
                    "reason": route.reason,
                },
            )
        )
    except Exception:  # noqa: BLE001 - a telemetry write must never block dispatch
        pass


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


def _worktree_ensure_for_launch(
    recorded_cwd: Path, agent_name: str, harness: str
) -> Optional[str]:
    """Resolve the launch cwd through the worktree verb (x-3f84 W5, change 5).

    The node's recorded cwd is the canonical checkout for every organically
    filed node, and launching there puts a code worker on the protected branch
    that sibling terminals share. ``fno workspace worktree ensure`` owns the
    policy resolution (per-project policy > global > harness-native); it
    prints the resolved root and exits 0, or prints nothing and exits non-zero
    on a refusal/misconfig - the caller HOLDS on that answer rather than
    falling back to canonical main. Returns the path to launch in (the repo
    root itself is the legal ``policy = never`` in-place answer), or None.
    """
    import subprocess

    from fno.agents.mux_spawn import _fno_bin

    try:
        repo = subprocess.run(
            ["git", "-C", str(recorded_cwd), "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if not recorded_cwd.is_dir():
        # A missing recorded cwd is the spawn's own error to surface (the old
        # behavior passed it through verbatim); it is not a worktree-policy
        # refusal, and holding here would break every scratch-cwd fixture.
        return str(recorded_cwd)
    if repo.returncode != 0:
        # ONLY a genuine "not a repository" answer means launch-in-place (a
        # vault project, worktree.policy=never by design). Any other git
        # failure - dubious ownership, a corrupted .git, a missing cwd - must
        # HOLD, not silently fall back to the canonical checkout this change
        # exists to keep workers off (review finding, x-3f84).
        if "not a git repository" in (repo.stderr or ""):
            return str(recorded_cwd)
        return None
    canonical = repo.stdout.strip()
    try:
        ensured = subprocess.run(
            [
                _fno_bin(),
                "workspace",
                "worktree",
                "ensure",
                "--repo",
                canonical,
                "--name",
                agent_name,
                "--harness",
                harness,
            ],
            capture_output=True,
            text=True,
            timeout=120,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if ensured.returncode != 0:
        return None
    return ensured.stdout.strip() or None


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
    rec: Optional[dict] = None
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
        rec = picked
        node_id = picked["id"]
        slug = picked.get("slug")
        cwd = picked.get("_resolved_cwd") or picked.get("cwd")
        priority = picked.get("priority")
        explicit = False

    if not isinstance(node_id, str) or not node_id:
        return {"outcome": "failed", "detail": "resolved node has no id"}
    parent = rec.get("parent") if isinstance(rec, dict) else None
    parent_id = parent.strip() if isinstance(parent, str) and parent.strip() else None

    # 1b. Quota-aware defer (x-5d3e). Only the ambient/autonomous default
    #     selection defers; an explicit --node dispatch always fires (LD#5).
    #     Fail-open: defer_dispatch off, p0, or UNKNOWN headroom -> proceed.
    #     The route decision is the SAME one `backlog advance` reads,
    #     so identical node + config + quota fixtures resolve to the identical
    #     destination tuple on both autonomous launchers.
    cutover = None
    cutover_command = ""
    if not explicit:
        from fno.agents.autonomous_route import (
            launch_is_pinned,
            select_autonomous_route,
        )

        route = select_autonomous_route(
            # An explicit --account IS the record this launch runs on, so probe
            # THAT one. Probing the active record instead would defer a healthy
            # pinned account because an unrelated active account is walled.
            provider_id=(account or "").strip() or _resolve_provider_id(cwd) or "",
            priority=priority,
            # The same pin rule `backlog advance` applies, so the two launchers
            # cannot disagree about whether a launch is rerouteable. An explicit
            # --account is a human's billing choice: quota policy may still defer
            # behind it, but must never reroute off it.
            pinned=launch_is_pinned(
                picked,
                account=account,
                node_cwd=cwd,
                # This verb hosts a claude pane and hardcodes that harness
                # below, so config.dispatch.harness is not a choice it honors -
                # pinning on it would block a cutover to protect nothing.
                honors_config_harness=False,
            ),
            node_cwd=cwd,
            node_id=node_id,
        )
        if route.action == "cutover":
            # Render the destination's own command HERE, before any claim or
            # reservation is taken: an unresolvable harness must fall back to the
            # defer floor rather than reach the spawn with a claude command.
            cutover_command = _cutover_command(route.harness, node_id)
            if cutover_command:
                cutover = route
            elif not route.defer_fallback:
                route = dataclasses.replace(route, action="stay")
            else:
                route = dataclasses.replace(route, action="defer")
        if route.action == "defer":
            # The selector already weighed both reroutes - a combo cutover and
            # launch-time account picking - so a defer that survives it is the
            # real floor. This used to re-check the account picker here, which
            # made identical fixtures defer under `backlog advance` and launch
            # under this verb.
            _emit_quota_deferred(
                node_id, route.source_record, route.window or "", route.retry_at
            )
            return {
                "outcome": "quota-deferred",
                "node": node_id,
                "slug": slug or "",
                "provider": route.source_record,
                "headroom": route.window or "",
                "retry_at": route.retry_at,
            }

    # 2. The SHARED family-2 pre-birth guard (x-3f84 W5, plan change 1): the
    #    same `_spawn_guard_decision` every `fno agents spawn` passes, not a
    #    hand-rolled `_node_dispatch_block_reason` + raw `acquire_claim` pair.
    #    It takes the `dispatch:<id>` reservation (closing the same-node race:
    #    two fast leader+g both resolve _next_node to the same node, and only
    #    the O_EXCL winner proceeds) AND the handover `node:<id>` claim (so the
    #    node reads as worked from dispatch), and its verdict keeps this verb's
    #    one-JSON-outcome contract: map a non-dispatchable verdict onto the
    #    outcome vocabulary instead of printing.
    from fno.agents.cli import _spawn_guard_decision
    from fno.claims.cli import HANDOVER_HOLDER_PREFIX

    guard, _guard_exit = _spawn_guard_decision(
        node_id,
        f"dispatch-one:{os.getpid()}",
        cwd=cwd,
        handover_holder=f"{HANDOVER_HOLDER_PREFIX}{_worker_agent_name(node_id, slug)}",
    )
    if guard.get("verdict") != "dispatchable":
        reason = str(guard.get("reason") or guard.get("verdict") or "unknown")
        if reason in ("already-claimed", "reservation-held"):
            outcome = "already-dispatching"
        elif guard.get("verdict") in ("error", "corrupted"):
            # An infrastructure fault (claims store unreadable, corrupted
            # claim) is a FAILURE, not a benign no-op class: the old raised
            # path answered exit 1 and the mux's failed arm renders the
            # detail, so an exit-0 verdict here would read as success to any
            # caller keying on the exit code (review finding, x-3f84).
            outcome = "failed"
        else:
            outcome = reason
        return {
            "outcome": outcome,
            "node": node_id,
            "slug": slug or "",
            "detail": str(guard.get("detail") or reason)[:200] or None,
        }
    dispatch_key = str(guard.get("reservation_key") or f"dispatch:{node_id}")
    dispatch_holder = str(guard.get("reservation_holder") or f"dispatch-one:{os.getpid()}")
    node_claim = (
        (str(guard["node_claim_key"]), str(guard["node_claim_holder"]))
        if guard.get("node_claim_key")
        else None
    )

    # 3. The spawn gate, then the spawn. Any exit from here releases every
    #    hold taken so far, so the node stays re-dispatchable - never a phantom
    #    reservation holding the node. On success dispatch:<id> is left to
    #    TTL-expire (bridges the boot window until the worker owns node:<id>).
    #
    #    The gate call below is the consolidation itself (x-3f84 W5, plan
    #    change 2): dispatch's workers now count against agents.max_live, the
    #    ONE fleet ceiling, instead of the private parallel-lane slot max_live
    #    never saw. A full fleet queues inside run_gate (or refuses with its
    #    own exit code on BaseException re-raise), so `lanes-full` left this
    #    verb's vocabulary for good.
    #
    #    Everything below - the gate, provenance, the cutover render, the
    #    refusal carrier - runs with a hold taken, so an exception there leaks
    #    just as a failed spawn does. And the guard catches BaseException, not
    #    just Exception: GateRefused subclasses SystemExit, which is a
    #    BaseException, so an `except Exception` lets a gate refusal walk out
    #    still holding the claim it was refused for - the same release shape as
    #    the run_gate call site in fno/agents/cli.py. A BaseException is
    #    re-raised rather than folded into a verdict, so a refusal keeps its
    #    own exit code and an interrupt still interrupts.
    #    Idempotent and best-effort per hold, for two reasons that both end in
    #    a leak. It is called from an early return INSIDE the try and from the
    #    handlers, so a release that raises on the early-return path re-enters
    #    through `except Exception` and releases a second time - and a second
    #    release can free a claim another spawner has since taken. A raise on
    #    the way out of the handler escapes it entirely and leaks both holds,
    #    which is the failure this whole guard exists to prevent. So the flag
    #    makes the second call a no-op, and one broken hold never blocks the
    #    other's release. A release that genuinely fails is reported, never
    #    swallowed silently: the TTL is the backstop and a human needs the line.
    from fno.agents.cli import _release_dispatch_claims

    released = False

    def _release_both() -> None:
        nonlocal released
        if released:
            return
        released = True
        try:
            _release_dispatch_claims((dispatch_key, dispatch_holder), node_claim)
        except Exception as exc:  # noqa: BLE001 - a release fault must not mask the real error
            # Reported, never silent: the TTL is the backstop and a human needs
            # the line (the same contract the shared helper holds per claim).
            print(
                f"dispatch one: could not release the claims for {node_id}: "
                f"{exc}. They hold until their TTLs expire",
                file=sys.stderr,
            )

    try:
        # The spawn gate (plan change 2): the SAME gate `fno agents spawn`
        # runs. route_provider stays None here because dispatch resolves no
        # model route today - the provider-budget dimension applies the day it
        # does, through this same call.
        from fno.agents.spawn_gate import run_gate

        # no_wait: prefix+g is an interactive keystroke, and the operator
        # wants an answer now - a silent detached task parked for the full
        # 10-minute queue timeout (the old instant lanes-full replaced by a
        # block) is the worse trade. A full fleet answers instantly with the
        # gate's own refusal exit code and its stderr reason.
        gate = run_gate(
            _worker_agent_name(node_id, slug),
            "pane",
            no_wait=True,
        )
        # Everything from here to the spawn runs with the gate HELD (pane
        # substrate keeps the mutex until the registry row exists), so every
        # exit path below - early return, spawn failure, refusal - releases
        # it through this finally. run_gate refuses with its own exit code
        # BEFORE this block, so a refusal never reaches it holding nothing.
        try:
            workdir = Path(cwd) if cwd else Path.cwd()
            # (x-c914) Stamp the birth account into the pane provenance (FNO_ACCOUNT)
            # when routed, so the mux server reads it back for the sideline account
            # glyph - a managed account shares ~/.claude, so the roster can't
            # distinguish it, but the pane's own env can (Locked Decision 5: pane env,
            # not the registry schema).
            provenance = resolve_provenance(node_id, slug)
            if node_claim is not None:
                # The worker proves it is the intended successor by naming this
                # holder back - env, never argv, the same contract as the spawn
                # path. Without it the pane clears the key and the worker cannot
                # rebind node:<id> until the launch-window claim expires
                # (review finding on the W5 cutover).
                provenance["FNO_NODE_CLAIM_HOLDER"] = node_claim[1]
            if account:
                provenance["FNO_ACCOUNT"] = account
            # A cutover replaces all three parts of the launch together (harness,
            # command, credential overlay); passing one without the others is the
            # wrong-billing / wrong-binary launch the selector exists to prevent.
            spawn_harness = "claude"
            # Same posture render as the cutover destination: through the resolver, so
            # the flag form (and any per-harness surface) comes from ONE template
            # (harness_map._AUTONOMOUS_COMMAND), not a second hardcoded string that
            # drifts when the token changes shape (x-9d11).
            message = _cutover_command(spawn_harness, node_id)
            if cutover is not None:
                spawn_harness = cutover.harness or "claude"
                message = cutover_command
                account_env = cutover.account_env
                provenance["FNO_ACCOUNT"] = cutover.record_id or ""
            # x-9d11 mechanical refusal carrier: the flag in the message is the
            # attributable carrier; the pane env is the backstop, so a worker that
            # never passes the flag through still folds the refusal at init.
            if not message:
                # _cutover_command's contract: empty = stage nothing. An unresolvable
                # target command must never spawn a billed pane with an empty prompt
                # (review round 5) - release both holds so the node stays grabbable.
                _release_both()
                return {
                    "outcome": "failed",
                    "node": node_id,
                    "slug": slug or "",
                    "detail": "target command unresolvable (dispatch_command refused); nothing spawned",
                }
            from fno.agents.harness_map import message_carries_no_merge

            if message_carries_no_merge(message):
                provenance["TARGET_NO_MERGE"] = "1"
            # The launch cwd is NOT the node's recorded cwd: for every organically
            # filed node that is the canonical checkout on the protected branch
            # (plan change 5, x-3f84 W5). Route through the worktree resolver; an
            # empty answer is a policy refusal or a misconfig, so HOLD - falling
            # back to canonical main is the exact launch this replaces. A result
            # equal to the repo root is the legal `worktree.policy = "never"` case
            # and launches in place. setup-worktree.sh stays caller-side (the
            # shellout-drift gate bars package code from repo-root scripts); the
            # worker's own `fno do target start` heals .fno state in the worktree.
            ensured = _worktree_ensure_for_launch(
                workdir, _worker_agent_name(node_id, slug), spawn_harness
            )
            if ensured is None:
                _release_both()
                return {
                    "outcome": "failed",
                    "node": node_id,
                    "slug": slug or "",
                    "detail": (
                        "worktree ensure refused or misconfigured; holding the node "
                        "rather than launching on canonical main"
                    ),
                }
            workdir = Path(ensured)
            # The guard rides into the spawn as provider_gate, so the provider
            # admission the pane consumes is one the caller actually obtained
            # (AC2-EDGE) - not the ungated launch dispatch used to perform.
            if parent_id is None:
                result = dispatch_spawn_pane(
                    name=_worker_agent_name(node_id, slug),
                    message=message,
                    provider=spawn_harness,
                    cwd=workdir,
                    session=session,
                    provenance=provenance,
                    account_env=account_env,
                    provider_gate=gate,
                )
            else:
                result = dispatch_spawn_pane(
                    name=_worker_agent_name(node_id, slug),
                    message=message,
                    provider=spawn_harness,
                    cwd=workdir,
                    session=session,
                    provenance=provenance,
                    account_env=account_env,
                    provider_gate=gate,
                    tab=parent_id,
                )
        finally:
            # The gate's claims go back once the registry row exists (or the
            # spawn failed): the row carries the count from here, the same
            # moment cmd_spawn releases its guard.
            gate.release()
    except Exception as exc:  # noqa: BLE001 - DispatchAskError or any spawn error
        _release_both()
        return {"outcome": "failed", "node": node_id, "slug": slug or "", "detail": str(exc)[:200]}
    except BaseException:
        _release_both()
        raise
    if cutover is not None:
        # Post-spawn only: a route decision is not a completed cutover.
        _emit_failover(node_id, cutover)
    # `launched` used to be declared from pane creation alone: this return had
    # no field capable of carrying a doubt, so a worker that never reached its
    # provider was indistinguishable from a healthy one. A confirmed-dead pane
    # now raises out of the spawn above (exit 13) into the `failed` return, and
    # `bound` separates a live-but-unbound worker from a bound one.
    # The seed doubt has to reach THIS caller too. `cmd_spawn` surfaces
    # `unattempted` and `unknown` as exit 22, and both mean the target command
    # may never have been submitted, so a bare `launched` said more than this
    # function knows.
    #
    # What this does and does NOT do. It reports the doubt, and `dispatch_notice`
    # renders it, so an operator watching the mux sees "seed unverified" instead
    # of a clean "dispatched". It does NOT release the reservation: the pane may
    # well be running, and dropping the hold on a live worker is the failure this
    # whole branch exists to prevent. Whether an unverified seed should also
    # release is a behaviour question with a live pane on the other side of it,
    # and it is not answered here.
    #
    # `seed == "submitted"` is NOT sufficient on its own, and the day the seed
    # word stopped carrying pane doubt is the day that became true. An argv seed
    # onto a pane whose frame could not be read now reports `submitted`, which is
    # honest about the payload and says nothing about whether anything is left to
    # run it. Certifying that as verified would hand every dispatcher a false
    # `seed_verified: true` - a worse lie than the `unattempted` this change
    # removed, because it reads as proof rather than as an absence.
    seed = getattr(result, "seed", None)
    observation = getattr(result, "pane_observation", None)
    return {
        "outcome": "launched",
        "node": node_id,
        "slug": slug or "",
        "pane_id": result.pane_id,
        "bound": result.bound,
        "seed": seed,
        "pane_observation": observation,
        "seed_verified": seed == "submitted" and observation != "unreadable",
    }
