"""fno agents dispatch: pick the next ready node and launch it (x-6f77).

The mux's ``leader+g`` ("grab work") shells here. One verb, one JSON verdict,
so the Rust caller renders a notice from a single exec instead of stitching
``fno backlog next`` + spawn + cap checks itself. Since x-e53e this verb owns
no launch of its own: it is node selection plus preference resolution plus one
``fno agents spawn`` call, the one launcher.

- selection: ``advance._next_node`` (the same board order ``fno backlog next`` uses)
- preferences: ``agents.node_dispatch.resolve_node_spawn`` (the ONE resolver
  the advance path reads too - harness, model, route, account, permission
  mode, the worker name, and the seed render)
- launch: ``fno agents spawn --node <id> --substrate pane`` into THIS session,
  which takes the shared family-2 guard (the ``dispatch:<id>`` reservation and
  the handover ``node:<id>`` claim), the spawn gate, and the placement lease

Never double-claims: the spawn door owns the guard, and the spawned worker's
own ``fno do target start`` re-anchors the node claim to its lifecycle.
"""

from __future__ import annotations

import dataclasses
import json
import re
import subprocess
from pathlib import Path
from typing import Optional

import typer

from fno import _subprocess_util
from fno.backlog.advance import (
    _next_node,
)

dispatch_app = typer.Typer(no_args_is_help=True, help="Dispatch ready work into mux panes.")


@dispatch_app.callback()
def _dispatch_callback() -> None:
    """No-op: keeps Typer from collapsing the single-command sub-app (a one-@command
    app otherwise swallows the ``next`` subcommand name)."""


def _dispatch_next_impl(
    server: Optional[str],
    session_legacy: Optional[str],
    node: Optional[str],
    project: Optional[str],
    account: Optional[str],
    json_output: bool,
) -> None:
    from fno._flag_aliases import merge_deprecated_alias

    session = merge_deprecated_alias(
        server,
        session_legacy,
        canonical_flag="--server",
        legacy_flag="--mux-session",
    )
    if session is None:
        typer.echo("fno agents dispatch next: --server is required")
        raise typer.Exit(code=2)
    verdict = _dispatch_one(session=session, node=node, project=project, account=account)
    if json_output:
        typer.echo(json.dumps(verdict))
    else:
        line = verdict["outcome"]
        if verdict.get("node"):
            line += f" {verdict['node']}"
        typer.echo(line)
    raise typer.Exit(code=0 if verdict["outcome"] != "failed" else 1)


@dispatch_app.command("next")
def cmd_next(
    server: Optional[str] = typer.Option(
        None, "--server", help="Mux server to spawn the pane into (FNO_SERVER)."
    ),
    session_legacy: Optional[str] = typer.Option(
        None,
        "--mux-session",
        hidden=True,
        help="Deprecated alias for --server.",
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
    """Dispatch the next ready node into a new pane on SERVER, through the one
    launcher (``fno agents spawn``).

    Named for what it does since x-e53e: this verb SELECTS and RESOLVES; the
    spawn door launches. Verdict ``outcome`` is one of ``launched | no-work |
    already-dispatching | quota-deferred | failed`` (plus the guard's own
    refusal reasons). A full fleet no longer returns a verdict: the spawn gate
    queues inside the door or refuses with its own exit code. Exit 0 for
    everything but ``failed``.
    """
    _dispatch_next_impl(server, session_legacy, node, project, account, json_output)


@dispatch_app.command("one", hidden=True)
def cmd_one(
    server: Optional[str] = typer.Option(
        None, "--server", help="Mux server to spawn the pane into (FNO_SERVER)."
    ),
    session_legacy: Optional[str] = typer.Option(
        None,
        "--mux-session",
        hidden=True,
        help="Deprecated alias for --server.",
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
    """Deprecated alias for ``dispatch next`` (x-e53e): the name predates the
    launcher collapse and answered "one of what?". Removed after one release."""
    _dispatch_next_impl(server, session_legacy, node, project, account, json_output)


@dispatch_app.command("resolve")
def cmd_resolve(
    harness: Optional[str] = typer.Option(
        None, "--harness", help="Target harness (claude|codex|gemini|agy|opencode). Default: config.dispatch.harness > claude."
    ),
    substrate: Optional[str] = typer.Option(
        None,
        "--substrate",
        help="bg|headless|pane. Default: per-harness (claude=bg, else headless; the thread lane must be journey-proven).",
    ),
    node: Optional[str] = typer.Option(
        None, "--node", "--id", help="Node id substituted into the command's {id}. Absent = template returned literally."
    ),
    command: Optional[str] = typer.Option(
        None, "--command", help="Command template. Default: config.dispatch.command > '/target --no-merge {id}'."
    ),
    verb: Optional[str] = typer.Option(
        None, "--verb", help="Node dispatch verb (validated against config.dispatch.allowed_verbs or config.dispatch.verb_registry); assembled as '<verb> {id}' for allowlisted verbs, rendered from its descriptor for registry verbs. Wins over --command's config/builtin default."
    ),
    brief: Optional[str] = typer.Option(
        None, "--brief", help="Node dispatch brief; returned in env.TARGET_BRIEF (never the command line). Capped at 8 KB."
    ),
    merge_posture: Optional[str] = typer.Option(
        None,
        "--merge-posture",
        help=(
            "no-merge|allow|from-config (x-8151): the resolver owns the "
            "--no-merge carrier. from-config reads config.auto_merge.grant "
            "from this cwd (errors degrade to no-merge)."
        ),
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
    # x-ebd2: the node ALWAYS loads under --node, explicit brief or not - the
    # lifecycle verb derives from the node's plan rung and difficulty, which an
    # explicit brief does not carry. The brief chain itself stays demand-driven
    # (an explicit --brief is rung 1 and is never overridden).
    rec = _lookup_node(node) if node else None
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
        from fno.graph.ladder import plan_rung as _node_plan_rung

        return resolve_dispatch(
            harness=target_harness,
            substrate=substrate,
            node_id=node,
            command=command,
            # The stored verb rides as audit input: out-of-family keeps
            # declared precedence; a family value reconciles through the table.
            verb=verb or _stored_verb(rec),
            # x-ebd2: node lifecycle context; the derived verb is the phase
            # authority and the stage table reads its profile row.
            difficulty=(rec or {}).get("difficulty") if node else None,
            plan_rung=_node_plan_rung(rec).value if node and rec else None,
            brief=brief,
            merge_posture=merge_posture,
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
        for key in ("harness", "substrate", "route", "command", "command_surface"):
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


@dispatch_app.command("family")
def cmd_family(
    message: str = typer.Option(
        ...,
        "--message",
        "-m",
        help="The payload whose FIRST token is tested for /target-family membership.",
    ),
) -> None:
    """Is this message a /target-family command? Prints ``family`` or
    ``other``, exit 0 either way - the same nothing-at-runtime contract as
    ``dispatch resolve``. The vocabulary is the canonical merge_posture table;
    this verb is its only shell-readable surface, so the shell scripts ask
    instead of carrying hand-copied pattern lists (the copies drifted)."""
    from fno.agents.harness_map import is_target_family

    typer.echo("family" if is_target_family(message) else "other")
    raise typer.Exit(code=0)


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
        from fno.adapters.providers.loader import record_harness
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
        # here worth probing: proceed as configured. An unknown record still
        # gets probed: skipping quota policy on a read failure is the change
        # that could dispatch onto a walled account.
        pinned_harness = (harness or "").strip()
        if pinned_harness:
            rec_harness = record_harness(provider_id, Path(cwd) if cwd else None)
            if rec_harness and rec_harness != pinned_harness:
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


def _stored_verb(rec: Optional[dict]) -> Optional[str]:
    """The node's stored dispatch_verb as resolver audit input (x-ebd2)."""
    return str((rec or {}).get("dispatch_verb") or "").strip() or None


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
    that sibling terminals share. ``fno agents workspace worktree ensure`` owns the
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
    #    The overlay itself is the spawn door's to apply: --account rides argv
    #    and cmd_spawn resolves it where the harness is exec'd.
    if account:
        from fno.agents.account_env import (
            AccountResolutionError,
            resolve_account_overlay,
        )

        try:
            resolve_account_overlay(account)
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
            # The same pin rule `backlog advance` applies (the default: a
            # launch the config harness answers for is pinned). x-e53e deleted
            # this verb's hardcoded claude, so config.dispatch.harness IS a
            # choice it honors - a cutover must never override it.
            pinned=launch_is_pinned(picked, account=account, node_cwd=cwd),
            node_cwd=cwd,
            node_id=node_id,
        )
        if route.action == "cutover":
            # The destination command is the resolver's render now (x-e53e
            # change 3): an unresolvable destination harness fails the spawn
            # door's own resolve instead of a pre-render fallback here.
            cutover = route
        elif route.action == "defer":
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

    # 2. Resolve every launch preference through the ONE resolver (x-e53e
    #    change 3): harness, model, route, account, permission mode, the
    #    worker name, and the seed render. The pane substrate and THIS mux
    #    session are the two deliberate pins below; nothing else is pinned
    #    by this file.
    from fno.agents.harness_map import DispatchResolveError
    from fno.agents.node_dispatch import node_spawn_argv, resolve_node_spawn
    from fno.backlog.advance import (
        SpawnError,
        _gate_refusal_detail,
    )

    try:
        args = resolve_node_spawn(
            node_id,
            cwd,
            slug,
            node=rec,
            harness=(cutover.harness if cutover is not None else None),
            dispatch_account=(cutover.record_id if cutover is not None else None),
            caller="dispatch-next",
        )
    except (SpawnError, DispatchResolveError) as exc:
        return {"outcome": "failed", "node": node_id, "slug": slug or "", "detail": str(exc)[:200]}

    # The launch cwd is NOT the node's recorded cwd (for organically filed
    # nodes that is canonical main); route through the worktree resolver and
    # HOLD on an empty answer. A repo-root answer is the legal
    # `worktree.policy = "never"` case; the worker's own `fno do target start`
    # heals .fno state in the worktree.
    ensured = _worktree_ensure_for_launch(
        Path(cwd) if cwd else Path.cwd(), args.agent_name, args.harness
    )
    if ensured is None:
        return {
            "outcome": "failed",
            "node": node_id,
            "slug": slug or "",
            "detail": (
                "worktree ensure refused or misconfigured; holding the node "
                "rather than launching on canonical main"
            ),
        }

    # 3. Shell the ONE launcher. The spawn door takes the family-2 guard (the
    #    `dispatch:<id>` reservation closing the same-node race, plus the
    #    handover `node:<id>` claim), runs the spawn gate (the ONE fleet
    #    ceiling, queueing or refusing with its own exit code), builds the
    #    provenance with the handover holder, and hosts the pane.
    extra: list[str] = ["--mux-session", session, "--no-wait"]
    if account:
        extra += ["--account", account]
    if parent_id is not None:
        extra += ["--tab", parent_id]
    cmd = [
        *_subprocess_util.fno_py_cmd(),
        "agents", "spawn",
        *node_spawn_argv(args, substrate="pane", cwd=ensured, extra=tuple(extra)),
    ]
    # A quota cutover must not change who may merge (the old render forced
    # --no-merge into the command). The x-9d11 env carrier does it here: the
    # door's own merge-posture logic reads the env beside the message flag.
    run_env = dict(args.env)
    if cutover is not None:
        run_env["TARGET_NO_MERGE"] = "1"
    try:
        proc = subprocess.run(
            cmd, capture_output=True, text=True, timeout=600, env=run_env
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return {"outcome": "failed", "node": node_id, "slug": slug or "", "detail": str(exc)[:200]}
    if proc.returncode != 0:
        stderr = (proc.stderr or "").strip()
        # The door's family-2 guard refused: map its reason onto this verb's
        # outcome vocabulary, exactly as the in-process guard mapping did.
        if proc.returncode == 2 and "node dispatch refused" in stderr:
            verdict_m = re.search(r"verdict=([^\s;]+)", stderr)
            reason_m = re.search(r"reason=([^\s;]+)", stderr)
            verdict = verdict_m.group(1) if verdict_m else ""
            reason = reason_m.group(1) if reason_m else ""
            if reason in ("already-claimed", "reservation-held") or verdict == "already-running":
                outcome = "already-dispatching"
            elif verdict in ("error", "corrupted"):
                # An infrastructure fault (claims store unreadable, corrupted
                # claim) is a FAILURE, not a benign no-op class: the mux's
                # failed arm renders the detail, so an exit-0 verdict here
                # would read as success to any caller keying on the exit code.
                outcome = "failed"
            else:
                outcome = reason or verdict or "failed"
            return {
                "outcome": outcome,
                "node": node_id,
                "slug": slug or "",
                "detail": stderr[:200] or None,
            }
        # A gate refusal keeps its own contract: the door prints the gate
        # receipt JSON to stdout and exits with the gate's code. Re-emit it
        # verbatim and keep the exit code - the pre-port GateRefused (a
        # SystemExit) propagated the same way.
        stdout_head = (proc.stdout or "").strip()
        if stdout_head.startswith("{") and stdout_head.endswith("}") and '"outcome"' not in stdout_head:
            typer.echo(stdout_head)
            raise SystemExit(proc.returncode)
        detail = _gate_refusal_detail(stderr or proc.stdout or "")
        return {
            "outcome": "failed",
            "node": node_id,
            "slug": slug or "",
            "detail": (detail or f"fno agents spawn exited {proc.returncode}")[:200],
        }
    if cutover is not None:
        # Post-spawn only: a route decision is not a completed cutover.
        _emit_failover(node_id, cutover)
    # The pane receipt (one JSON line on stdout) carries the launch facts this
    # verdict reports. The seed doubt reaches THIS caller exactly as before:
    # `seed_verified` is false the moment the frame could not be read, and
    # `dispatch_notice` renders the doubt to the operator.
    receipt: Optional[dict] = None
    for line in (proc.stdout or "").splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            parsed = json.loads(line)
        except ValueError:
            continue
        if isinstance(parsed, dict) and "pane_id" in parsed:
            receipt = parsed
    if receipt is None:
        return {
            "outcome": "failed",
            "node": node_id,
            "slug": slug or "",
            "detail": (
                (proc.stdout or proc.stderr or "").strip()[:200]
                or "fno agents spawn exited 0 with no pane receipt"
            ),
        }
    seed = receipt.get("seed")
    observation = receipt.get("pane_observation")
    return {
        "outcome": "launched",
        "node": node_id,
        "slug": slug or "",
        "pane_id": receipt.get("pane_id"),
        "bound": receipt.get("bound"),
        "seed": seed,
        "pane_observation": observation,
        "seed_verified": seed == "submitted" and observation != "unreadable",
    }
