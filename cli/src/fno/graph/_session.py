"""Blueprint session lifecycle verbs: stamp, open, close, reap.

The close releases the spawn-handover claim or the blueprint-session claim it
was opened under and repoints dispatch_verb at the launch verb, so a finished
blueprint names what runs next.
"""

import json
import os
from typing import Optional

import typer

# The constant lives in claims beside the other two holder prefixes.
from fno.claims.core import BLUEPRINT_HOLDER_PREFIX, HANDOVER_HOLDER_PREFIX
from fno.config._dispatch_verbs import parse_verb_token

_BLUEPRINT_CLAIM_TTL_MS = 2 * 60 * 60 * 1000


def _graph_path():
    """Resolve through fno.graph.cli at call time (same seam as tests patch)."""
    from fno.graph.cli import _graph_path as _resolve

    return _resolve()


# -- session add (lifecycle provenance, ) --

session_app = typer.Typer(
    name="session",
    help="Append-only lifecycle session provenance ().",
    no_args_is_help=True,
    add_completion=False,
)


def _release_into(receipt: dict, claim_key: str, holder: str) -> None:
    """Release exactly OUR holder and stamp the receipt; a close never fails
    on its release - the claim just waits out its TTL."""
    from fno.claims.core import release_claim
    from fno.claims.io import claims_root_for

    try:
        released = release_claim(
            claim_key, holder, strict=True, root=claims_root_for(claim_key)
        )
        receipt["claim_released"] = bool(released)
        if released:
            receipt["claim_holder"] = holder
    except Exception as exc:  # noqa: BLE001 - a close never fails on its release
        receipt["claim_released"] = False
        typer.echo(
            f"session close: {claim_key} not released: "
            f"{type(exc).__name__}: {exc}. It stays held until its TTL expires.",
            err=True,
        )


def _own_handover_holder(session_id: str) -> str:
    """The spawn-handover holder fno agents spawn took for this session, or ""."""
    env = (os.environ.get("FNO_NODE_CLAIM_HOLDER") or "").strip()
    if env.startswith(HANDOVER_HOLDER_PREFIX):
        return env
    from fno.claims.self_identity import _roster_name_for_session

    name = _roster_name_for_session(session_id)
    return HANDOVER_HOLDER_PREFIX + name if name else ""


def _plan_claims(plan_path: str) -> "set[str]":
    """Delegate to the single parser (``_intake.plan_claims``).

    Kept as a thin alias because the stamp-guard call site reads better with
    a local name; the implementation lives in one place so this reader and
    collision's self-exclusion default cannot diverge.
    """
    from fno.graph._intake import plan_claims

    return plan_claims(plan_path)


@session_app.callback()
def _session_callback() -> None:
    """Keep ``add`` a real subcommand (a single-command Typer app auto-collapses,
    which would parse ``session add <node>`` with ``add`` as the node)."""


@session_app.command("add")
def cmd_session_add(
    node: Optional[str] = typer.Argument(
        None, help="Node id / slug / bare-hex to stamp (mutually exclusive with --pr-number)."
    ),
    phase: str = typer.Option(
        ..., "--phase", help="Lifecycle phase: think|blueprint|do|review|ship."
    ),
    pr: Optional[int] = typer.Option(
        None,
        "--pr-number",
        help="Resolve the UNIQUE node carrying this PR number instead "
        "of passing NODE (rejects 0 or multiple matches; never fans out).",
    ),
    repo: Optional[str] = typer.Option(
        None,
        "--repo",
        help="Scope --pr-number resolution to an <owner>/<repo> slug "
        "(pr_number is not unique across repos in a cross-project graph). "
        "Omit and the verb resolves the current checkout's slug itself.",
    ),
    harness: Optional[str] = typer.Option(
        None, "--harness", help="Override harness (default: ambient session identity)."
    ),
    session_id: Optional[str] = typer.Option(
        None, "--session-id", help="Override session id (default: ambient session identity)."
    ),
    effort: Optional[str] = typer.Option(
        None, "--effort", help="Selected reasoning effort, passed through verbatim."
    ),
    ended_at: Optional[str] = typer.Option(
        None,
        "--ended-at",
        "--at",
        help="ISO-8601 UTC instant the phase ended. Omit when there is no honest end "
        "to record (a row opened mid-session); explicit for backfill of completed work.",
    ),
    started_at: Optional[str] = typer.Option(
        None,
        "--started-at",
        "--claimed-at",
        help="ISO-8601 UTC instant the work began; lands on the "
        "row so it bounds the window with --ended-at. Honest for every "
        "phase (a think row starts but claims nothing).",
    ),
    require_session: Optional[str] = typer.Option(
        None,
        "--require-session",
        help="Skip (exit 0) unless the ambient session id equals "
        "this. Identity-continuity guard for stale manifests.",
    ),
    guard_plan: Optional[str] = typer.Option(
        None,
        "--guard-plan",
        help="Skip (exit 0) if this plan's frontmatter `claims:` names "
        "a DIFFERENT node. Requires NODE (not --pr-number).",
    ),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit the result as JSON."),
) -> None:
    """Stamp a node with a lifecycle phase record (idempotent, append-only).
    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    Self-close spelling (the owning session): session add <node> --phase <phase> --ended-at <ISO-8601 now>.
    """
    from fno.graph.fuzzy import resolve_node
    from fno.graph.api import wire_rows
    from fno.graph.store import (
        append_session_record,
        find_nodes_for_pr,
        stamp_session_for_pr,
    )

    def _open_row_to_end(node_id: str):
        """The open (phase, session) row an --ended-at append would close,
        read only when an end is being recorded - every other call pays
        nothing."""
        if ended_at is None:
            return None
        from fno.graph.statuses import is_open_phase_row

        for entry in wire_rows(path=_graph_path()) or []:
            if not (isinstance(entry, dict) and entry.get("id") == node_id):
                continue
            for row in entry.get("sessions") or []:
                if (isinstance(row, dict) and isinstance(row.get("phase"), str)
                        and row["phase"] == phase
                        and row.get("session_id") == eff_session
                        and is_open_phase_row(row, phase)):
                    return row
        return None

    def _refuse_foreign_row(node_id: str, prior) -> None:
        # Ending another live session's open row is the synthesized-death move
        # reap-open owns (it demands proof of death); only the owning session
        # ends its own row here. Keyed on session identity, never the
        # --session-id override: a guard a caller satisfies by asserting its
        # own answer is not a guard.
        if prior is None or eff_session == (ident.session_id or "").strip():
            return
        owner_harness = prior.get("harness") or eff_harness
        typer.echo(
            f"session add: {owner_harness}:{eff_session} owns an open {phase} row "
            f"on {node_id}; only that session ends it here. A dead session's row "
            f"closes with: fno backlog session reap-open {node_id} --phase {phase} "
            f"--harness {owner_harness} --session-id {eff_session} "
            "(after proving death).",
            err=True,
        )
        raise typer.Exit(code=2)

    if (node is None) == (pr is None):
        typer.echo("session add: pass exactly one of NODE or --pr-number.", err=True)
        raise typer.Exit(code=2)
    # Refused, never ignored: the guard compares against the node the row lands
    # on, and the --pr-number path resolves that only after it has stamped.
    if guard_plan is not None and pr is not None:
        typer.echo("session add: --guard-plan requires NODE, not --pr-number.", err=True)
        raise typer.Exit(code=2)

    who = node if node is not None else f"pr#{pr}"

    def _skip(reason: str, node_id: "str | None" = None) -> None:
        typer.echo(f"session add: {reason} (target={who} phase={phase}). Skipped.", err=True)
        if json_out:
            typer.echo(
                json.dumps(
                    {
                        "node_id": node_id,
                        "status": "skipped",
                        "reason": reason,
                        "phase": phase,
                        "harness": eff_harness,
                        "session_id": eff_session,
                        "added": False,
                    }
                )
            )

    from fno.claims.self_identity import resolve_self_identity

    ident = resolve_self_identity()
    eff_harness = (harness or ident.harness or "").strip()
    eff_session = (session_id or ident.session_id or "").strip()
    if not eff_harness or not eff_session:
        typer.echo(
            f"session add: no ambient identity for {who} phase={phase}; "
            "pass --harness/--session-id or run inside a session. Skipped.",
            err=True,
        )
        raise typer.Exit(code=2)

    # Identity continuity: the caller vouches for whose session this manifest
    # belongs to; a mismatch means it belongs to a different conversation (the
    # stale-manifest squatter), so the record is not this session's to write.
    # Compared against the AMBIENT id, never the --session-id override: a guard a
    # caller can satisfy by asserting its own answer is not a guard. No ambient
    # identity at all therefore also skips - continuity is unprovable.
    if require_session is not None:
        # The row must record the identity the guard actually checked. Allowing an
        # override would verify one identity and permanently write another, which
        # is the same self-certification hole in a different shape. Refused rather
        # than ignored, like --guard-plan with --pr-number.
        if session_id is not None or harness is not None:
            typer.echo(
                "session add: --require-session cannot be combined with "
                "--session-id/--harness (it would verify one identity and "
                "record another).",
                err=True,
            )
            raise typer.Exit(code=2)
        ambient = (ident.session_id or "").strip()
        if ambient != require_session.strip():
            return _skip(f"ambient session {ambient!r} != required {require_session.strip()!r}")

    # After the identity guard: resolution shells out to git and possibly gh, and
    # a run with no identity is about to skip anyway.
    if pr is not None and repo is None:
        from fno.graph._reconcile import resolve_current_repo_slug

        repo = resolve_current_repo_slug()
        if repo is None:
            typer.echo(
                f"session add: could not resolve this checkout's repo slug for pr#{pr}; "
                "matching on the bare PR number (skips on cross-repo ambiguity).",
                err=True,
            )
        # No bare-number fallback once a slug resolves. The graph is GLOBAL and
        # cross-project, so a url-less node is unattributable to ANY repo - a
        # fallback cannot tell "this repo's legacy node" from "another project's
        # legacy node with the same PR number", and stamping the latter is the
        # wrong-node write repo scoping exists to prevent. Refusing to guess
        # costs a stamp on a legacy node; guessing costs a corrupted one, and
        # the skip is now LOUD (it names the candidates), so nothing is silent.

    prior_open = None
    try:
        if pr is not None:
            # Both paths resolve one node before the stamp: find_nodes_for_pr
            # yields exactly one id here or stamp_session_for_pr skips below.
            if ended_at is not None:
                single = find_nodes_for_pr(_graph_path(), pr, repo=repo)
                if len(single) == 1:
                    prior_open = _open_row_to_end(single[0])
                    _refuse_foreign_row(single[0], prior_open)
            node_id, status = stamp_session_for_pr(
                _graph_path(),
                pr,
                phase=phase,
                harness=eff_harness,
                session_id=eff_session,
                ended_at=ended_at,
                effort=effort,
                started_at=started_at,
                repo=repo,
            )
            if status in ("no-node", "ambiguous"):
                cands = find_nodes_for_pr(_graph_path(), pr, repo=repo)
                detail = f" (candidates: {', '.join(cands)})" if cands else ""
                repair = ""
                if status == "no-node":
                    # Resolution matches the node's STORED pr_number, so a node
                    # whose PR was never stamped (typical of a session killed
                    # before ship) is invisible - the exact state the repair
                    # path is needed in. Name the two ways out instead of
                    # leaving the operator stuck; a branch guess would risk
                    # stamping the wrong node, so refuse and explain.
                    repair = (
                        " A node whose PR was never stamped is invisible here. "
                        "Link it with `fno backlog update <node-id> "
                        f"--pr-number {pr}`, or pass the node id directly: "
                        f"`fno backlog session add <node-id> --phase {phase}`."
                    )
                typer.echo(
                    f"session add: PR {pr} maps to {status}{detail} (phase={phase}); "
                    f"resolution is exact and never fans out.{repair} Skipped.",
                    err=True,
                )
                if json_out:
                    typer.echo(
                        json.dumps(
                            {
                                "node_id": None,
                                "status": status,
                                "phase": phase,
                                "harness": eff_harness,
                                "session_id": eff_session,
                                "added": False,
                                "candidates": cands,
                            }
                        )
                    )
                return
            added = status == "added"
        else:
            # session add is a mutation verb: local-store resolution, guarded
            # against external backends by the shared refusal (task 4.2), not
            # the display-reader seam.
            match = resolve_node(node, wire_rows(path=_graph_path()))
            if match.kind != "exact":
                typer.echo(f"session add: no node matches {node!r} (phase={phase}).", err=True)
                raise typer.Exit(code=2)
            node_id = match.candidates[0]["id"]
            # Plan agreement (mirrors /execute Step 1.5): only a POSITIVE disagreement
            # skips. An unreadable plan or an absent `claims:` is agreement-
            # unknown, and absent evidence of conflict is not conflict.
            #
            # This is the one guard that does NOT fail closed, so it says so out
            # loud when it could not evaluate. Otherwise an install whose
            # plan_path is systematically stale (plan moved, vault unmounted)
            # runs with G3 disabled and no operator signal anywhere.
            if guard_plan is not None:
                claims = _plan_claims(guard_plan)
                if not claims:
                    typer.echo(
                        f"session add: plan {guard_plan} is unreadable or declares no "
                        f"claims; agreement not evaluated for {node_id}.",
                        err=True,
                    )
                elif node_id not in claims:
                    return _skip(
                        f"plan {guard_plan} claims {sorted(claims)} != node {node_id}",
                        node_id=node_id,
                    )
            prior_open = _open_row_to_end(node_id)
            _refuse_foreign_row(node_id, prior_open)
            found, added = append_session_record(
                _graph_path(),
                node_id,
                phase=phase,
                harness=eff_harness,
                session_id=eff_session,
                ended_at=ended_at,
                effort=effort,
                started_at=started_at,
            )
            if not found:
                typer.echo(f"session add: node {node_id} not found (phase={phase}).", err=True)
                raise typer.Exit(code=2)
    except ValueError as exc:
        typer.echo(f"session add: {exc} (target={who} phase={phase})", err=True)
        raise typer.Exit(code=2)

    # An honest self-close is not a duplicate: an open row existed before and
    # reads closed after, so the receipt says what happened. A backfill (no
    # prior open row) keeps `recorded`; a re-close keeps `already recorded`.
    # A harness mismatch appends a second row and leaves the first open, so it
    # is not an end.
    ended_existing = (
        prior_open is not None
        and prior_open.get("harness") == eff_harness
        and ended_at is not None
    )
    if json_out:
        typer.echo(
            json.dumps(
                {
                    "node_id": node_id,
                    "status": (
                        "ended" if ended_existing
                        else ("added" if added else "duplicate")
                    ),
                    "phase": phase,
                    "harness": eff_harness,
                    "session_id": eff_session,
                    "added": added,
                }
            )
        )
    elif ended_existing:
        typer.echo(f"ended {phase} {eff_harness}:{eff_session} on {node_id}")
    else:
        state = "recorded" if added else "already recorded"
        typer.echo(f"{state} {phase} {eff_harness}:{eff_session} on {node_id}")


@session_app.command("open")
def cmd_session_open(
    node: str = typer.Argument(..., help="Node id / slug / bare-hex."),
    harness: Optional[str] = typer.Option(None, "--harness"),
    session_id: Optional[str] = typer.Option(None, "--session-id"),
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit the open receipt as JSON."
    ),
) -> None:
    """Hold node:<id> under blueprint-session:<id> for this session's planner.

    The open takes only the claim; the close writes the lifecycle row and
    releases. A planner running between open and close is visible to every
    dispatch gate, so a second planner on the same node is refused here.
    """
    from fno.claims.core import (
        ClaimContended,
        ClaimCorrupted,
        ClaimGoneAway,
        ClaimHeldByOther,
        acquire_claim,
        claim_status,
    )
    from fno.claims.io import claims_root_for
    from fno.claims.self_identity import resolve_self_identity
    from fno.graph.fuzzy import resolve_node
    from fno.graph.api import wire_rows

    ident = resolve_self_identity()
    eff_harness = (harness or ident.harness or "").strip()
    eff_session = (session_id or ident.session_id or "").strip()
    if not eff_harness or not eff_session:
        typer.echo(
            f"session open: no ambient identity for {node}; run inside a session.",
            err=True,
        )
        raise typer.Exit(code=2)
    match = resolve_node(node, wire_rows(path=_graph_path()))
    if match.kind != "exact":
        typer.echo(f"session open: no exact node matches {node!r}.", err=True)
        raise typer.Exit(code=2)
    node_id = match.candidates[0]["id"]
    claim_key = f"node:{node_id}"
    holder = BLUEPRINT_HOLDER_PREFIX + eff_session
    existing = claim_status(claim_key, root=claims_root_for(claim_key))
    if existing.get("state") != "free" and existing.get("holder") == holder:
        typer.echo(
            f"session open: node:{node_id} is already open for this session ({holder}).",
            err=True,
        )
        raise typer.Exit(code=1)
    own = _own_handover_holder(eff_session)
    if own and existing.get("holder") == own and existing.get("state") in ("live", "suspect"):
        # fno agents spawn claimed the node for this worker: plan under that claim.
        receipt = {"node_id": node_id, "status": "joined", "claim_key": claim_key, "holder": own}
        typer.echo(json.dumps(receipt) if json_out else f"joined {node_id} holder={own}")
        return
    try:
        from fno.claims.session_pid import resolve_session_pid

        pid = resolve_session_pid()
    except Exception:  # noqa: BLE001 - no durable pid; the lease and session witness hold the claim
        pid = None
    try:
        claim = acquire_claim(
            claim_key,
            holder,
            reason=f"blueprint session for {node_id}",
            ttl_ms=_BLUEPRINT_CLAIM_TTL_MS,
            pid=pid,
            pid_unavailable=pid is None,
            harness=eff_harness,
            harness_session_id=eff_session,
            root=claims_root_for(claim_key),
        )
    except ClaimHeldByOther as exc:
        typer.echo(
            f"session open: node:{node_id} held by {exc.holder} (pid={exc.pid}); "
            "no planner started.",
            err=True,
        )
        raise typer.Exit(code=1)
    except (ClaimCorrupted, ClaimGoneAway, ClaimContended) as exc:
        typer.echo(
            f"session open: node:{node_id} could not be claimed: "
            f"{type(exc).__name__}: {exc}.",
            err=True,
        )
        raise typer.Exit(code=3)
    receipt = {
        "node_id": node_id,
        "status": "opened",
        "claim_key": claim_key,
        "holder": holder,
        "harness": eff_harness,
        "session_id": eff_session,
        "acquired_at": claim.acquired_at,
    }
    if json_out:
        typer.echo(json.dumps(receipt))
    else:
        typer.echo(f"opened {node_id} holder={holder}")


@session_app.command("close")
def cmd_session_close(
    node: str = typer.Argument(..., help="Node id / slug / bare-hex."),
    summary: str = typer.Option(..., "--summary", help="Completion summary for the blueprint."),
    launch: str = typer.Option(..., "--launch", help="Exact launch command for the next phase."),
    harness: Optional[str] = typer.Option(None, "--harness"),
    session_id: Optional[str] = typer.Option(None, "--session-id"),
    started_at: Optional[str] = typer.Option(None, "--started-at"),
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit the completion receipt as JSON."
    ),
) -> None:
    """Close the blueprint phase with one identity-guarded completion receipt.

    The close writes the blueprint lifecycle row with an honest end, then emits
    the summary and exact launch line. Missing identity is a hard refusal: the
    close cannot claim completion while leaving provenance unresolved.
    """
    from datetime import datetime, timezone

    from fno.claims.self_identity import resolve_self_identity
    from fno.graph.fuzzy import resolve_node
    from fno.graph.api import wire_rows
    from fno.graph.store import append_session_record, commit_rows_via_store

    summary = summary.strip()
    launch = launch.strip()
    if not summary or not launch:
        typer.echo("session close: summary and launch must be non-empty.", err=True)
        raise typer.Exit(code=2)
    ident = resolve_self_identity()
    eff_harness = (harness or ident.harness or "").strip()
    eff_session = (session_id or ident.session_id or "").strip()
    if not eff_harness or not eff_session:
        typer.echo(
            f"session close: no ambient identity for {node}; "
            "pass --harness/--session-id or run inside a session.",
            err=True,
        )
        raise typer.Exit(code=2)
    match = resolve_node(node, wire_rows(path=_graph_path()))
    if match.kind != "exact":
        typer.echo(f"session close: no exact node matches {node!r}.", err=True)
        raise typer.Exit(code=2)
    node_id = match.candidates[0]["id"]
    ended_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    # A blueprint-session claim bounds the planning window: its acquire time
    # is the row's started_at unless the caller pinned one. The backfilled
    # subagent rows this closes for good carried started_at: null.
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for

    claim_key = f"node:{node_id}"
    claim = claim_status(claim_key, root=claims_root_for(claim_key))
    blueprint_holder = BLUEPRINT_HOLDER_PREFIX + eff_session
    blueprint_held = (
        claim.get("state") != "free" and claim.get("holder") == blueprint_holder
    )
    acquired_at = claim.get("acquired_at")
    # A planner that joined its spawn's handover claim started at that claim.
    own_claim = blueprint_held or (
        claim.get("state") != "free" and claim.get("holder") == _own_handover_holder(eff_session)
    )
    if own_claim and started_at is None and isinstance(acquired_at, int):
        started_at = datetime.fromtimestamp(acquired_at / 1000, tz=timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        )
    try:
        found, added = append_session_record(
            _graph_path(),
            node_id,
            phase="blueprint",
            harness=eff_harness,
            session_id=eff_session,
            ended_at=ended_at,
            started_at=started_at,
        )
    except ValueError as exc:
        typer.echo(f"session close: {exc}", err=True)
        raise typer.Exit(code=2)
    if not found:
        typer.echo(f"session close: node {node_id} disappeared before close.", err=True)
        raise typer.Exit(code=2)
    receipt = {
        "node_id": node_id,
        "status": "closed",
        "phase": "blueprint",
        "harness": eff_harness,
        "session_id": eff_session,
        "summary": summary,
        "launch": launch,
        "ended_at": ended_at,
        "added": added,
    }
    # Name the next verb BEFORE the release below: the release wakes
    # dispatchers, and one must never resolve the blueprint slot just ended.
    launch_verb = launch.split()[0]
    stored_verb = launch_verb
    parsed_launch = parse_verb_token(launch_verb)
    if parsed_launch and parsed_launch[1]:
        stored_verb = f"/fno:{parsed_launch[0]}"
    if parsed_launch and parsed_launch[1]:

        def _write_dispatch_verb(entries):
            for entry in entries:
                if entry.get("id") == node_id and entry.get("dispatch_verb") != stored_verb:
                    entry["dispatch_verb"] = stored_verb
                    break
            return entries

        commit_rows_via_store(_graph_path(), _write_dispatch_verb)
    else:
        typer.echo(
            f"session close: dispatch_verb not written: launch token "
            f"{launch_verb!r} is not a plugin-qualified verb.",
            err=True,
        )
    # A spawn dispatch acquires node:<id> under spawn-handover:<worker> and
    # this close is the only terminal that lifecycle has. Release exactly OUR
    # holder, never the key: a successor target session may already hold the
    # claim under its own after rebinding it at init. A claim has one holder,
    # so at most one branch matches.
    holder = (os.environ.get("FNO_NODE_CLAIM_HOLDER") or "").strip()
    claim_key = f"node:{node_id}"
    if holder.startswith("spawn-handover:"):
        _release_into(receipt, claim_key, holder)
    elif blueprint_held:
        _release_into(receipt, claim_key, blueprint_holder)
    else:
        # The env export cannot reach a daemon-forked worker, so the closing
        # session may not carry its own handover holder. Resolve the worker
        # name the registry binds to this session and release exactly that
        # holder; any other holder stays held.
        handover = _own_handover_holder(eff_session)
        if handover and claim.get("holder") == handover:
            _release_into(receipt, claim_key, handover)
        else:
            receipt["claim_released"] = False
    if receipt.get("claim_released"):
        # The retired claim mirror stamped the plan-bound node ready when
        # this close released its own claim; the store-era port keeps the
        # transition at the write path, so no read has to guess it back.
        def _settle_ready(entries):
            for entry in entries:
                if entry.get("id") != node_id:
                    continue
                open_do = any(
                    row.get("phase") == "do" and not row.get("ended_at")
                    for row in entry.get("sessions") or []
                    if isinstance(row, dict)
                )
                terminal = any(
                    entry.get(field)
                    for field in ("completed_at", "superseded_by", "deferred_at", "pr_number")
                )
                if entry.get("status") == "in_progress" and not open_do and not terminal:
                    entry["status"] = "ready"
                break
            return entries

        commit_rows_via_store(_graph_path(), _settle_ready)
    if json_out:
        typer.echo(json.dumps(receipt))
    else:
        typer.echo(f"blueprint closed {node_id} ({eff_harness}:{eff_session})")
        typer.echo(f"summary: {summary}")
        typer.echo(f"launch: {launch}")


@session_app.command(
    "backfill",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def cmd_session_backfill(ctx: typer.Context) -> None:
    """Fill missing session starts and ends from transcripts and merge commits. Never overwrites a stamp.

    A dry run by default. --apply writes the fills; --json (-J) prints the per-phase counts as JSON.
    """
    import subprocess

    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo("session backfill: the fno-agents binary was not found.", err=True)
        raise typer.Exit(code=2)
    argv = [str(binary), "session-backfill", "--graph", str(_graph_path()), *ctx.args]
    raise typer.Exit(code=subprocess.run(argv, check=False).returncode)


@session_app.command("reap-open")
def cmd_session_reap_open(
    node: "str | None" = typer.Argument(
        None,
        help=(
            "Node id / slug / bare-hex. Omit to settle EVERY node holding an "
            "open row for the identity (the death-cascade form)."
        ),
    ),
    harness: str = typer.Option(..., "--harness", help="Harness owning the dead session."),
    session_id: str = typer.Option(..., "--session-id", help="Dead harness session id."),
    phase: str = typer.Option(
        "do",
        "--phase",
        help=(
            "Lifecycle phase of the open row. 'do' removes the row (it wedges "
            "node status); any other phase (a spawn-opened review row) fills "
            "ended_at and keeps the provenance; 'all' settles every open row "
            "carrying the identity (the death-cascade spelling)."
        ),
    ),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit a structured receipt."),
) -> None:
    """Reap one exact open session row after the observer proves session death; the reap sweep settles a done+merged node's open do row on its own, so this verb is the hand path for every other case, including a node still in flight. Without a node the identity form settles every node holding an open row for the session."""
    from fno.graph.fuzzy import resolve_node
    from fno.graph.statuses import is_open_do_row, is_open_phase_row
    from fno.graph import api as graph_api
    from fno.graph.api import wire_rows
    from fno.graph.store import reap_open_session_record
    from fno.graph.types import SESSION_PHASES

    if node is None:
        try:
            receipt = reap_open_session_record(
                _graph_path(), None, phase=phase, harness=harness, session_id=session_id
            )
        except (ValueError, OSError, RuntimeError) as exc:
            typer.echo(f"session reap-open: {exc}", err=True)
            raise typer.Exit(code=2)
        if not receipt.get("settled"):
            typer.echo(
                "session reap-open: no open row carries that identity on any node.",
                err=True,
            )
            raise typer.Exit(code=1)
        if json_out:
            typer.echo(json.dumps(receipt, sort_keys=True))
        else:
            nodes = ", ".join(receipt.get("node_ids") or [])
            typer.echo(
                f"settled {nodes or 'nothing'}: row_removed={receipt['row_removed']} "
                f"row_closed={receipt.get('row_closed')}"
            )
        return

    entries = wire_rows(path=_graph_path())
    match = resolve_node(node, entries)
    if match.kind != "exact":
        typer.echo(f"session reap-open: no exact node matches {node!r}.", err=True)
        raise typer.Exit(code=2)
    node_id = match.candidates[0]["id"]
    try:
        receipt = reap_open_session_record(
            _graph_path(), node_id, phase=phase, harness=harness, session_id=session_id
        )
    except (ValueError, OSError, RuntimeError) as exc:
        typer.echo(f"session reap-open: {exc}", err=True)
        raise typer.Exit(code=2)

    reread = graph_api.node(node_id, path=_graph_path())
    rebound = reread.model_dump(by_alias=True) if reread else None
    if rebound is None:
        typer.echo(f"session reap-open: node {node_id} disappeared on read-back.", err=True)
        raise typer.Exit(code=1)
    rows = rebound.get("sessions") or []
    want_phases = sorted(SESSION_PHASES) if phase == "all" else [phase]
    matching_open = any(
        any(is_open_phase_row(row, ph) for ph in want_phases)
        and (row.get("harness"), row.get("session_id")) == (harness.strip(), session_id.strip())
        for row in rows
    )
    remaining = sum(is_open_do_row(row) for row in rows)
    higher_precedence = (
        any(
            rebound.get(field)
            for field in ("completed_at", "superseded_by", "deferred_at", "pr_number")
        )
        # A blocked node outranks in_progress; the store never persists it.
        or rebound.get("persisted_status") == "blocked"
        or rebound.get("status") == "blocked"
    )
    expected_in_progress = bool(rebound.get("locked_by")) or remaining > 0
    status_ok = higher_precedence or (
        (rebound.get("status") == "in_progress") == expected_in_progress
    )
    if matching_open or not status_ok:
        typer.echo(
            f"session reap-open: read-back did not settle {node_id} "
            f"(matching_open={matching_open}, status={rebound.get('status')!r}, "
            f"remaining_open_do={remaining}).",
            err=True,
        )
        raise typer.Exit(code=1)

    receipt.update(
        {
            "node_id": node_id,
            "settled": True,
            "status_after": rebound.get("status"),
            "remaining_open_do": remaining,
        }
    )
    if json_out:
        typer.echo(json.dumps(receipt, sort_keys=True))
    else:
        typer.echo(
            f"settled {node_id}: row_removed={receipt['row_removed']} "
            f"row_closed={receipt.get('row_closed')} "
            f"status={receipt['status_after']} remaining_open_do={remaining}"
        )
