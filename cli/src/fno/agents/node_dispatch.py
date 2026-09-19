"""One resolver for node-dispatch spawn preferences (change 2).

Everything `backlog.advance._spawn_worker` computed above its argv build is
:func:`resolve_node_spawn`, so every node-dispatching caller reads ONE answer
and `fno agents spawn` stays the only launcher. Callers build argv via
:func:`node_spawn_argv` and shell the door; none launches a peer.
"""

from __future__ import annotations

import dataclasses
import os
import sys
from pathlib import Path
from typing import Optional

from fno.config._dispatch_verbs import canonical_verb_key, parse_verb_token


@dataclasses.dataclass
class NodeSpawnArgs:
    """The resolved launch preferences one node dispatch needs.

    ``harness`` rides ``--harness`` (account record aliases included);
    ``resolved_harness`` is the resolver's answer, the one the claude-only argv
    gates read. ``route`` is the grid lane's pick, ``resolved_route`` the stage
    table's verb lane. ``env`` is the subprocess env: base minus a stale
    ``TARGET_NO_MERGE``, plus the resolver's answer and ``extra_env``.
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
    session_phase: Optional[str]
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
; helpers are imported at CALL time from advance so the suite's
    monkeypatches keep firing. Raises the same ``SpawnError``s;
    ``DispatchResolveError`` propagates to the caller's spawn-failure path.
    """
    from fno.backlog.advance import (
        SpawnError,
        _grid_lane_for,
        _launch_harness_axis,
        _worker_agent_name,
    )
    from fno.agents.naming import verb_code_for

    is_reconcile = bool(reconcile_manifest)
    if is_reconcile:
        if source is not None and source != "rd":
            raise SpawnError(
                f"refusing to dispatch {node_id}: source {source!r} with a "
                "reconcile manifest is an impossible pair; the de-stub pass "
                "is always rd."
            )
        source = "rd"
    node_verb = (verb or "").strip() or None
    # The node dict IS the verb evidence. A missing dict or a dict without
    # the key is a lossy projection: REFUSE before anything is spent.
    if not isinstance(node, dict):
        raise SpawnError(
            f"refusing to dispatch {node_id}: {caller} passed no node dict; "
            "the builtin path has no verb evidence."
        )
    if "dispatch_verb" not in node:
        raise SpawnError(
            f"refusing to dispatch {node_id}: the node dict {caller} passed "
            "carries no dispatch_verb key; the projection feeding this "
            "dispatcher is lossy (id x-1111); fix the projection, not the node."
        )
    verb_source = (
        "declared" if str(node.get("dispatch_verb") or "").strip() else "none-declared"
    )
    # the effective workflow verb. Reconcile bypasses (its explicit
    # command spells the de-stub pass).
    effective_verb: Optional[str] = None
    if not is_reconcile:
        effective_verb = node_effective_verb(node)
    # x-aaaa: the verb code resolves (and refuses) BEFORE the resolver.
    verb_code = "t" if is_reconcile else verb_code_for(effective_verb or node_verb)
    # A node's own raw pin is a sanctioned source (route_resolve reads the same
    # field), so fold it in before the grid consult: the gate below must see
    # every pin the node carries, whatever its door passed.
    if not (model or "").strip():
        model = (node.get("model") or "").strip() or None
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
    # An unpinned spawn bills the account's default model (measured 2026-09-17:
    # opus on this fleet), the silent substitution a routing law can never
    # survive. A dropped pin REFUSES; grid_why carries the resolver's terminal
    # verbatim when one exists.
    if not (model or "").strip():
        decline = f"; {grid_why}" if grid_why else ""
        raise SpawnError(
            f"refusing to dispatch {node_id}: no model survives resolution "
            f"(unpinned = the account default model){decline}; pin the node's "
            "model or repair the routing config, then retry."
        )
    # x-aaaa/ the name mints ONCE here - after the lane/model consult,
    # before spawn - so it carries the model tag, riding the receipt.
    agent_name = _worker_agent_name(
        node_id,
        node_slug,
        source=source,
        verb_code=verb_code,
        model=model,
    )

    # / node_cwd precedence, so a cross-project dispatch reads
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

    # / a node dispatch_verb takes the verb path (never a
    # merge); reconcile spells its own posture; with neither, the builtin rung
    # reads config.auto_merge.grant itself. DispatchResolveError propagates to
    # the caller's non-fatal spawn-failure path.
    from fno.agents import harness_map

    # One axis: `provider` is the harness under an older spelling, so it must
    # reach the resolver too, or the command follows the stage table instead.
    launch_axis = _launch_harness_axis(launch, node_cwd)
    # The receipt names the RESOLVED verb; verb_source keeps the
    # RAW state, canonicalized so receipt and command agree on the spelling.
    receipt_verb = effective_verb or node_verb or "builtin"
    if parse_verb_token(receipt_verb):
        receipt_verb = canonical_verb_key(receipt_verb)
    resolve_kwargs: dict = {
        "harness": ((harness or "").strip() or launch_axis or None),
        "node_id": node_id,
        "brief": (brief or None),
        "trigger": "autonomous",
        "settings": settings_obj,
    }
    if is_reconcile:
        # the refusal spelling is inserted by the shared vocabulary
        # helper, never a second hardcoded "--no-merge " string.
        resolve_kwargs["command"] = f"/target --reconcile {reconcile_manifest} {{id}}"
        if not allow_merge:
            resolve_kwargs["command"] = harness_map.inject_no_merge_into_command(
                resolve_kwargs["command"]
            )
    else:
        # the node's lifecycle context rides so the resolver derives
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

    # A registry verb is unknown to the spawn door's verb table: the
    # descriptor's declared phase rides the argv, else the door refuses a
    # --node spawn it cannot label. A config read failure degrades to None,
    # which is that refusal with its remedy - never a guessed label.
    descriptor_phase: Optional[str] = None
    try:
        from fno.config._dispatch_verbs import resolvable_verbs

        dispatch_cfg = getattr(settings_obj, "dispatch", None)
        registry = getattr(dispatch_cfg, "verb_registry", None) or {}
        allowed = getattr(dispatch_cfg, "allowed_verbs", None)
        declared = resolvable_verbs(registry, allowed).get(
            canonical_verb_key(effective_verb or node_verb or "")
        )
        descriptor_phase = (getattr(declared, "session_phase", None) or "").strip() or None
    except Exception:  # noqa: BLE001 - a config read never guesses a label
        descriptor_phase = None

    prov = launch or resolved["harness"]  # alias kept; resolver owns the default
    if launch_axis and launch_axis != resolved["harness"]:
        raise SpawnError(
            f"refusing to spawn {node_id}: --harness {prov!r} runs "
            f"{launch_axis!r} but the command is spelled for "
            f"{resolved['harness']!r} ({target_cmd!r}). Pass one axis."
        )

    # / explicit permission_mode > the operator's spawn default >
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
    # the resolver's env is AUTHORITATIVE for the merge posture, so a
    # stale inherited TARGET_NO_MERGE never survives into a successor the
    # resolver just granted allow-merge (review round 5).
    base_env = {k: v for k, v in os.environ.items() if k != "TARGET_NO_MERGE"}
    run_env = {**base_env, **merged_env} if merged_env else (base_env or None)
    if source in ("ac", "rd", "ab"):
        from fno.harness_identity import scrub_ambient_identity
        run_env = {**(run_env or {}), "FNO_SPAWN_TRIGGER": f"dispatch:{source}"}
        scrub_ambient_identity(run_env)

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
        session_phase=descriptor_phase,
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
    node-dispatching caller, so the claude-only gates cannot drift.
    Callers prepend the binary + verb; ``cwd`` rides before the model axis,
    the order the advance suite pins.
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
    # a per-node model pin rides as a spawn flag. Empty/None = provider
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
    #.
    if args.dispatch_account:
        cmd += ["--dispatch-account", args.dispatch_account]
    if extra:
        cmd += list(extra)
    # the worker-to-node join; without --node the registry row names
    # no node and no instrument can answer which worker is on which node.
    cmd += ["--node", args.node_id]
    if args.node_slug:
        cmd += ["--slug", args.node_slug]
    if args.session_phase:
        # a registry verb's declared phase. The spawn door refuses an
        # unlabeled --node spawn, so an outside verb must carry its label.
        cmd += ["--session-phase", args.session_phase]
    cmd += ["--name", args.agent_name, args.command]
    return cmd


@dataclasses.dataclass
class NodeSeed:
    """The rendered seed for a node-driven spawn (change 1), plus the
    launch workdir resolution change 1 moved into the spawn door."""

    node_id: str
    message: str
    slug: Optional[str]
    plan_path: Optional[str]
    env: dict
    receipt: dict
    recorded_cwd: Optional[str]

    def ensure_launch_workdir(self, harness: str) -> Optional[Path]:
        """The launch workdir for a node-seeded spawn with no explicit cwd
        source: the worktree ensure's answer, never the node's recorded cwd
        (the canonical checkout for every organically filed node). A ``None``
        answer already printed its hold line - the caller exits 2 and the node
        stays claimable rather than launching on canonical main."""
        return ensure_launch_workdir(self.recorded_cwd, self.node_id, harness)


def find_node_row(node: str) -> Optional[dict]:
    """The graph row for a node id or slug, or None on an unreadable graph.
    One lookup so the door, the seam and retask read the SAME row."""
    try:
        from fno.graph.load import load_graph

        for candidate in load_graph():
            if candidate.get("id") == node or candidate.get("slug") == node:
                return candidate
    except Exception:  # noqa: BLE001 - an unreadable graph cannot seed a spawn
        return None
    return None


def node_effective_verb(
    row: Optional[dict], *, node_id: Optional[str] = None
) -> Optional[str]:
    """The lifecycle table's answer for a node row, or None on abstain:
    one answer per node, shared by every door. Accepts a None row; raises
    DispatchResolveError on an unanswerable node."""
    from fno.agents import harness_map
    from fno.graph.ladder import plan_rung

    verb, _note = harness_map.resolve_effective_verb(
        verb=((row or {}).get("dispatch_verb") or "").strip() or None,
        difficulty=(row or {}).get("difficulty"),
        plan_rung=plan_rung(row).value,
        node_id=node_id if node_id is not None else (row or {}).get("id"),
    )
    return verb


def render_node_seed(node: str, *, harness: Optional[str]) -> Optional[NodeSeed]:
    """Render a node's seed (verb command + brief env) for the spawn door.

    Prints its own refusal and returns None when the node carries no
    dispatch_verb, or when the seed render refuses (an unanswerable lifecycle
    or an over-budget brief) - a truncated brief is never seeded.
    """
    from fno.agents.harness_map import DispatchResolveError, resolve_dispatch
    from fno.graph.ladder import plan_rung as _node_plan_rung
    from fno.provenance.autobrief import resolve_dispatch_brief

    seed_rec: Optional[dict] = find_node_row(node)
    seed_node_id = (seed_rec or {}).get("id") or node
    if not isinstance(seed_rec, dict) or not str(seed_rec.get("dispatch_verb") or "").strip():
        print(
            f"refusing node-seeded spawn: node {seed_node_id} carries no "
            "dispatch_verb and no message was typed; an idle worker holds "
            "a fleet slot and reads as alive. Encode one with `fno backlog "
            f"update {seed_node_id} --dispatch-verb <verb>`.",
            file=sys.stderr,
        )
        return None
    try:
        node_brief, node_brief_source = resolve_dispatch_brief(seed_rec)
        resolved_seed = resolve_dispatch(
            harness=harness,
            node_id=str(seed_node_id),
            verb=str(seed_rec.get("dispatch_verb")).strip(),
            difficulty=seed_rec.get("difficulty"),
            plan_rung=_node_plan_rung(seed_rec).value,
            brief=node_brief,
            trigger="autonomous",
        )
    except DispatchResolveError as exc:
        # An unanswerable lifecycle or an explicit >8KB brief (the one
        # failure the brief chain leaves to this gate) refuses here, before
        # any worker exists; a truncated brief is never seeded.
        print(str(exc), file=sys.stderr)
        return None
    return NodeSeed(
        node_id=str(seed_node_id),
        message=resolved_seed["command"],
        slug=seed_rec.get("slug"),
        plan_path=seed_rec.get("plan_path"),
        env=resolved_seed.get("env") or {},
        receipt={
            "verb_source": "declared",
            "brief_source": node_brief_source,
        },
        recorded_cwd=seed_rec.get("_resolved_cwd") or seed_rec.get("cwd"),
    )


def ensure_launch_workdir(
    recorded_cwd: Optional[str], node_id: str, harness: str
) -> Optional[Path]:
    """Resolve the launch workdir through the launch-workdir seam verb
    (ported to Rust), printing the hold line on a refusal or a transport
    failure. The verb keys the ensure on the NODE id, so a node-seeded spawn
    resumes the node's existing worktree instead of minting a worker-named
    tree beside it."""
    from fno.rust_binary import VerbUnavailable, verb_call

    payload = {
        "recorded_cwd": str(Path(recorded_cwd) if recorded_cwd else Path.cwd()),
        "node": node_id,
        "harness": harness,
    }
    try:
        answer = verb_call(
            "launch-workdir",
            payload,
            VerbUnavailable,
            timeout=150,
            passthrough_stderr=True,
        )
    except VerbUnavailable:
        answer = None
    if answer is None or "hold" in answer:
        print(
            f"fno agents spawn: worktree ensure refused or misconfigured for "
            f"{node_id}; holding the node rather than launching on canonical "
            "main",
            file=sys.stderr,
        )
        return None
    return Path(answer["workdir"])
