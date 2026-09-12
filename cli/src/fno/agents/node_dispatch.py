"""One resolver for node-dispatch spawn preferences (x-e53e change 2).

Everything `backlog.advance._spawn_worker` computed above its argv build is
:func:`resolve_node_spawn`, so every node-dispatching caller reads ONE answer
and `fno agents spawn` stays the only launcher. Callers build argv via
:func:`node_spawn_argv` and shell the door; none launches a peer.
"""

from __future__ import annotations

import dataclasses
import os
import sys
from typing import Optional


@dataclasses.dataclass
class NodeSpawnArgs:
    """The resolved launch preferences one node dispatch needs.

    ``harness`` rides ``--harness`` (account record aliases included);
    ``resolved_harness`` is the resolver's answer, the one the claude-only argv
    gates read. ``route`` is the grid lane's pick, ``resolved_route`` the stage
    table's verb lane. ``env`` is the subprocess env: base minus a stale
    ``TARGET_NO_MERGE``, plus the resolver's answer and caller ``extra_env``.
    """

    node_id: str
    node_cwd: Optional[str]
    node_slug: Optional[str]
    harness: str
    resolved_harness: str
    substrate: str
    command: str
    model: Optional[str]
    route: Optional[str]
    resolved_route: Optional[str]
    account: Optional[str]
    dispatch_account: Optional[str]
    permission_mode: str
    agent_name: str
    vendor: Optional[str]
    verb: str
    verb_source: str
    grid_reason: Optional[str]
    decision: list
    is_reconcile: bool
    env: dict


def resolve_node_spawn(
    node_id: str,
    node_cwd: Optional[str],
    node_slug: Optional[str],
    *,
    node: Optional[dict] = None,
    reconcile_manifest: Optional[str] = None,
    model: Optional[str] = None,
    provider: Optional[str] = None,
    vendor: Optional[str] = None,
    grid_route: Optional[str] = None,
    grid_account: Optional[str] = None,
    harness: Optional[str] = None,
    verb: Optional[str] = None,
    brief: Optional[str] = None,
    extra_env: Optional[dict] = None,
    dispatch_account: Optional[str] = None,
    permission_mode: Optional[str] = None,
    grid_reason: Optional[str] = None,
    source: Optional[str] = None,
    caller: str = "unknown",
) -> NodeSpawnArgs:
    """Resolve every node-dispatch preference except the launch itself.

    Lifted verbatim from ``backlog.advance._spawn_worker``'s preference half
    (x-e53e); helpers are imported at CALL time from advance so the suite's
    monkeypatches on those names keep firing. Raises the same ``SpawnError``s;
    ``DispatchResolveError`` propagates to the caller's spawn-failure path.
    """
    from fno.backlog.advance import (
        SpawnError,
        _grid_lane_for,
        _launch_harness_axis,
        _node_effective_verb,
        _worker_agent_name,
    )
    from fno.agents.naming import verb_code_for

    is_reconcile = bool(reconcile_manifest)
    if is_reconcile:
        if source is not None and source != "rd":
            raise SpawnError(
                f"refusing to dispatch {node_id}: source {source!r} with a "
                "reconcile manifest is an impossible pair; the de-stub pass "
                "is always rd (x-84b2)."
            )
        source = "rd"
    node_verb = (verb or "").strip() or None
    # x-0961/x-ebd2: classify the RAW declaration from the DICT alone. A dict
    # without the key is a lossy projection: REFUSE before anything is spent.
    # A None node keeps its warning + path.
    if isinstance(node, dict) and "dispatch_verb" not in node:
        raise SpawnError(
            f"refusing to dispatch {node_id}: the node dict {caller} passed "
            "carries no dispatch_verb key; the projection feeding this "
            "dispatcher is lossy (x-0961); fix the projection, not the node."
        )
    if isinstance(node, dict):
        verb_source = (
            "declared" if str(node.get("dispatch_verb") or "").strip() else "none-declared"
        )
    else:
        verb_source = "field-absent"
        print(
            f"advance: WARNING: dispatching {node_id} with no node dict "
            f"({caller}); the builtin target path runs with no verb_source "
            "evidence (x-0961).",
            file=sys.stderr,
        )
    # x-ebd2: the effective workflow verb. Reconcile bypasses (its explicit
    # command spells the de-stub pass).
    effective_verb: Optional[str] = None
    if isinstance(node, dict) and not is_reconcile:
        effective_verb = _node_effective_verb(node)
    # x-84b2: the verb code resolves (and refuses) BEFORE the resolver, and
    # the name mints ONCE here, before spawn, riding the receipt.
    verb_code = "t" if is_reconcile else verb_code_for(effective_verb or node_verb)
    agent_name = _worker_agent_name(
        node_id,
        node_slug,
        source=source,
        verb_code=verb_code,
    )
    # --provider selects the account/record (or a bare kind like "claude"),
    # layer-separate from `harness` (the record's cli). NOT the launch harness:
    # defaulting it here once launched claude carrying codex syntax.
    launch = (provider or "").strip()

    # Capacity-grid deferral: the lane is picked HERE, at the seam that can
    # read live capacity (the spawned argv always carries --harness, so the
    # spawn-CLI grid can never fire on this path). An explicit harness skips
    # the consult; a caller that resolved the grid hands its answer in.
    grid_why: Optional[str] = grid_reason
    grid_lane_route: Optional[str] = grid_route
    grid_lane_account: Optional[str] = grid_account
    if harness is None:
        grid_harness, grid_model, grid_route_resolved, grid_account_resolved, grid_why = _grid_lane_for(
            node, model=model, provider=provider, verb=effective_verb
        )
        if grid_harness is not None:
            model = grid_model
            # The resolver must see the grid's harness or it resolves a
            # claude substrate/command for a codex spawn (bg is claude-only).
            harness = grid_harness
            grid_lane_route = grid_route_resolved
            grid_lane_account = grid_account_resolved

    # x-4391/x-4be1: node_cwd precedence, so a cross-project dispatch reads
    # the DEPENDENT node's config; the same settings object feeds the resolver
    # and the permission-mode read. Any read failure -> no-merge.
    settings_obj = None
    try:
        from pathlib import Path as _Path

        from fno.config import load_settings, load_settings_for_repo
        from fno.config.grant import auto_merge_grant

        settings_obj = (
            load_settings_for_repo(_Path(node_cwd)) if node_cwd else load_settings()
        )
    except Exception:  # noqa: BLE001 - unreadable config -> defaults below
        settings_obj = None
    # Read the grant in its OWN guard so a missing/odd block never disables the
    # independent permission-mode read that also consumes settings_obj. Only the
    # literal "dispatch" grants (a typo or a stub settings object never does).
    allow_merge = auto_merge_grant(settings_obj)

    # x-0676/x-8e59: a node dispatch_verb takes the verb path (never a
    # merge); reconcile spells its own posture; with neither, the builtin rung
    # reads config.auto_merge.grant itself. DispatchResolveError propagates to
    # the caller's non-fatal spawn-failure path.
    from fno.agents import harness_map

    # One axis: `provider` is the harness under an older spelling, so it must
    # reach the resolver too, or the command follows the stage table instead.
    launch_axis = _launch_harness_axis(launch, node_cwd)
    # The receipt names the RESOLVED verb (x-ebd2); verb_source keeps the
    # RAW state, canonicalized so receipt and command agree on the spelling.
    receipt_verb = effective_verb or node_verb or "builtin"
    if receipt_verb.startswith("/fno:"):
        receipt_verb = "/" + receipt_verb[len("/fno:"):]
    resolve_kwargs: dict = {
        "harness": ((harness or "").strip() or launch_axis or None),
        "node_id": node_id,
        "brief": (brief or None),
        "trigger": "autonomous",
        "settings": settings_obj,
    }
    if is_reconcile:
        # x-8151: the refusal spelling is inserted by the shared vocabulary
        # helper, never a second hardcoded "--no-merge " string.
        resolve_kwargs["command"] = f"/target --reconcile {reconcile_manifest} {{id}}"
        if not allow_merge:
            resolve_kwargs["command"] = harness_map.inject_no_merge_into_command(
                resolve_kwargs["command"]
            )
    else:
        # x-ebd2: the node's lifecycle context rides so the resolver derives
        if isinstance(node, dict):
            from fno.graph.ladder import plan_rung as _node_plan_rung

            resolve_kwargs["difficulty"] = node.get("difficulty")
            resolve_kwargs["plan_rung"] = _node_plan_rung(node).value
        if node_verb:
            resolve_kwargs["verb"] = node_verb
    resolved = harness_map.resolve_dispatch(**resolve_kwargs)
    substrate = resolved["substrate"]
    target_cmd = resolved["command"]
    spawn_env = resolved.get("env") or {}

    prov = launch or resolved["harness"]  # alias kept; resolver owns the default
    if launch_axis and launch_axis != resolved["harness"]:
        raise SpawnError(
            f"refusing to spawn {node_id}: --harness {prov!r} runs "
            f"{launch_axis!r} but the command is spelled for "
            f"{resolved['harness']!r} ({target_cmd!r}). Pass one axis."
        )

    # x-dfa4/x-7198: explicit permission_mode > the operator's spawn default >
    # the built-in unattended answer. Never unset for a claude dispatch below.
    mode = (permission_mode or "").strip()
    if not mode and settings_obj is not None:
        try:
            from fno.agents.spawn_defaults import SPAWN_PERMISSION_BUILTIN

            mode = (
                settings_obj.agents.defaults.permission_mode or ""
            ).strip() or SPAWN_PERMISSION_BUILTIN
        except Exception:  # noqa: BLE001 - fail-safe to unset (unchanged)
            mode = ""

    from fno.agents.account_env import STATE_ROOT_ENV_KEYS

    for key in sorted(STATE_ROOT_ENV_KEYS & set(extra_env or {})):
        raise SpawnError(
            f"refusing to put {key} on the `fno agents spawn` wrapper: footnote "
            f"resolves its own state root off {key}, so the worker's registry "
            "row, claim and events would land in the account's home where "
            "nothing looks. Pass the destination account as "
            "--dispatch-account <record> instead; the spawn front door applies "
            "the overlay where the harness is exec'd."
        )
    merged_env = {**spawn_env, **(extra_env or {})}
    # x-9d11: the resolver's env is AUTHORITATIVE for the merge posture, so a
    # stale inherited TARGET_NO_MERGE never survives into a successor the
    # resolver just granted allow-merge (review round 5).
    base_env = {k: v for k, v in os.environ.items() if k != "TARGET_NO_MERGE"}
    run_env = {**base_env, **merged_env} if merged_env else (base_env or None)

    return NodeSpawnArgs(
        node_id=node_id,
        node_cwd=node_cwd,
        node_slug=node_slug,
        harness=prov,
        resolved_harness=resolved["harness"],
        substrate=substrate,
        command=target_cmd,
        model=model,
        route=grid_lane_route,
        resolved_route=resolved.get("route"),
        account=grid_lane_account,
        dispatch_account=dispatch_account,
        permission_mode=mode,
        agent_name=agent_name,
        vendor=vendor,
        verb=receipt_verb,
        verb_source=verb_source,
        grid_reason=grid_why,
        decision=list(resolved.get("decision") or []),
        is_reconcile=is_reconcile,
        env=run_env or {},
    )


def node_spawn_argv(
    args: NodeSpawnArgs,
    *,
    substrate: Optional[str] = None,
    cwd: Optional[str] = None,
    extra: tuple = (),
) -> list[str]:
    """The ``fno agents spawn`` flags for resolved args: ONE builder for every
    node-dispatching caller (x-e53e), so the claude-only gates cannot drift.
    Callers prepend the binary + verb and pass placement flags via ``extra``;
    ``cwd`` rides before the model axis, the order the advance suite pins.
    """
    cmd = [
        "--harness", args.harness,
        "--substrate", substrate or args.substrate,
    ]
    if args.vendor:
        cmd += ["--provider", args.vendor]
    elif args.route:
        # The row's route owns vendor AND model as one fact; an explicit
        # dispatch-time vendor pin outranks it and is never replaced.
        cmd += ["--route", args.route]
    elif args.resolved_route and args.resolved_harness == "claude":
        # No grid pick: the stage table's verb lane route, or a routed claude
        # model dies on the default endpoint at first inference. Claude-gated:
        # --route is a claude-only axis at the spawn seam.
        cmd += ["--route", args.resolved_route]
    if args.account and args.resolved_harness == "claude":
        # The capacity pick read THIS account's quota; claude-only at the CLI.
        cmd += ["--account", args.account]
    elif args.account:
        print(
            f"advance: grid account {args.account!r} skipped "
            f"(claude-only, harness {args.resolved_harness!r})",
            file=sys.stderr,
        )
    if cwd:
        cmd += ["--cwd", cwd]
    else:
        cmd += ["--fresh"]
    # x-571f: a per-node model pin rides as a spawn flag. Empty/None = provider
    # default, byte-identical to today.
    if args.model:
        cmd += ["--model", args.model]
    # CLAUDE-ONLY (mirrors dispatch-node.sh): gate on the RESOLVED harness, so
    # a claude ACCOUNT record (ccm/ccr) still gets the flag - without it the
    # account-pinned worker hangs on a prompt (the bug the gate fixed).
    if args.permission_mode and args.resolved_harness == "claude":
        cmd += ["--permission-mode", args.permission_mode]
    # A cutover's destination account rides argv as a RECORD ID, never env:
    # the front door applies the overlay where the harness is exec'd, and a
    # HOME-carrying overlay would move the state root where nothing looks
    # (x-c33e).
    if args.dispatch_account:
        cmd += ["--dispatch-account", args.dispatch_account]
    if extra:
        cmd += list(extra)
    # x-0961: the worker-to-node join; without --node the registry row names
    # no node and no instrument can answer which worker is on which node.
    cmd += ["--node", args.node_id]
    if args.node_slug:
        cmd += ["--slug", args.node_slug]
    cmd += ["--name", args.agent_name, args.command]
    return cmd
