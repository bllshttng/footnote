"""fno claim - Typer surface for the work-claim verbs.

Exit codes:
    0  success
    1  ClaimHeldByOther, or acquire/refresh's own contention-retry
       exhaustion (both mean "transient, caller should retry later"); also
       `reap`'s own distinct overload of 1 - a reapable file's archive move
       could not be confirmed on re-read (see `reap`'s own docstring, not a
       retry signal)
    2  validation / input error
    3  ClaimCorrupted or ClaimGoneAway (race during operation)
    4  HolderMismatch (release/refresh wrong holder)

The structured output uses --json on each verb. Without --json, output is a
human-friendly summary on stdout; errors always go to stderr.

do provenance: why the claim verbs write it
-------------------------------------------
A node's `do` lifecycle row used to be written only at a clean terminal
(release --stamp-do, the finalize backstop, /do Step 1.5). A session killed
mid-phase reaches none of those, so the whole row was lost - including a
started_at that sat in its claim file the entire time. A node finished to an
open, green, attested PR read `sessions=[blueprint only]`, and a groom pass
would have redone it.

The claim is the one thing every worker touches at the start of work and again
at its end, so the row is bound to the claim's own lifecycle:

  acquire  -> OPEN the row (started_at = claim.acquired_at, no ended_at)
  release  -> CLOSE it (--stamp-do fills ended_at on the same row)

`append_session_record` completes a duplicate row by filling a timestamp the
first write omitted and never overwrites one, so open-then-close collapses to a
single row and a retried stamp is a no-op.

Three constraints shape the rest:

* Both reachable acquire paths must stamp. The CLI `acquire` verb (init's cold
  start) and target-start's in-process `_reacquire_node_claim` takeover are
  separate code paths; a stamp on one is decorative, since a session killed on
  the other still loses its row.
* The identity is the OWNED one, never the ambient env. `_owned_do_identity`
  reads the harness the claim was pinned to (init passes the proven --harness)
  and the session encoded in the holder, because ambient marker precedence would
  launder an inherited foreign marker into the row. Acquire and release share it
  so they always address the same row.
* Acquiring is not doing. A caller that takes the claim as a serialization step
  and only then validates can be refused after the row is open; it releases with
  --rollback-do, which removes an open row whose started_at matches this claim.
  A closed row, or one opened by an earlier real window under the same identity,
  is never touched.
"""
from __future__ import annotations

import json
import re
from pathlib import Path
from typing import List, NamedTuple, Optional

import typer

from .core import (
    ClaimContended,
    ClaimCorrupted,
    ClaimGoneAway,
    ClaimHeldByOther,
    ClaimValidationError,
    HolderMismatch,
    acquire_claim,
    claim_status,
    force_release_claim,
    list_claims_with_counts,
    reap_dead_claims,
    refresh_claim,
    release_claim,
)
from .io import dedup_claims_roots, global_claims_root
from fno.tombstones import tombstone_group_cls


cli = typer.Typer(
    name="claim",
    help="Work-claim coordination primitive",
    no_args_is_help=True,
    cls=tombstone_group_cls("claim"),
)


_TTL_PATTERN = re.compile(r"^\s*(\d+)\s*([smh]?)\s*$", re.IGNORECASE)


def _parse_ttl(value: Optional[str]) -> Optional[int]:
    """Convert "1h" / "30m" / "3600s" / "5000" into milliseconds.

    Empty string and None return None (caller decides default).
    Plain digits are interpreted as seconds, matching ``sleep`` convention.
    """
    if value is None or value == "":
        return None
    m = _TTL_PATTERN.match(value)
    if not m:
        raise typer.BadParameter(f"invalid TTL format: {value!r} (use '30m', '1h', '3600s')")
    n = int(m.group(1))
    unit = m.group(2).lower()
    if unit == "s" or unit == "":
        return n * 1000
    if unit == "m":
        return n * 60_000
    if unit == "h":
        return n * 3_600_000
    raise typer.BadParameter(f"unknown TTL unit: {unit!r}")


def _parse_metadata(value: str) -> dict:
    if not value:
        return {}
    try:
        parsed = json.loads(value)
    except json.JSONDecodeError as exc:
        raise typer.BadParameter(f"--metadata is not valid JSON: {exc}")
    if not isinstance(parsed, dict):
        raise typer.BadParameter("--metadata must be a JSON object")
    return parsed


#: Seconds the roster cross-check may spend shelling out to the harness.
#: A status read is interactive, so it must answer late-but-honestly
#: ("roster not consulted") rather than hang a king mid-decision.
_ROSTER_CROSSCHECK_TIMEOUT_S = 10.0


def _node_aware_root(key: str):
    """Resolve the claims root for a key (delegates to the shared helper).

    Global-id kinds (``node:``/``dispatch:``/``reconcile:``/``session:``) route to
    the global ``~/.fno/claims`` so operator commands work without the env var
    (ab-fcf9cec5); repo-local keys keep the cwd/env default. See
    :func:`fno.claims.io.claims_root_for` for the single source of truth.
    """
    from .io import claims_root_for

    return claims_root_for(key)


@cli.command()
def acquire(
    key: Optional[str] = typer.Argument(
        None, help="Lock key, e.g. node:ab-1234abcd. Omit when using --lane."
    ),
    holder: str = typer.Option("", "--holder", help="Symbolic owner string"),
    lane: Optional[str] = typer.Option(
        None,
        "--lane",
        help=(
            "Acquire a LANE SLOT for this lane id instead of a keyed claim "
            "(was `claim lane-acquire --lane-id`). Requires --max-lanes; "
            "takes no KEY and no --holder."
        ),
    ),
    max_lanes: Optional[int] = typer.Option(
        None, "--max-lanes", help="With --lane: concurrency cap (>=1; 1 == sequential)."
    ),
    reason: str = typer.Option("", "--reason", "-R", help="Optional rationale recorded in audit"),
    ttl: str = typer.Option("", "--ttl", help="TTL expression like 30m, 1h, 3600s (omit => PID-liveness)"),
    metadata: str = typer.Option("{}", "--metadata", help="JSON object passed verbatim"),
    pid: Optional[int] = typer.Option(
        None,
        "--pid",
        help=(
            "PID-liveness anchor (omit => nearest agent session, else this process's "
            "PID). Pin the claim to a LONG-LIVED owner (e.g. a stream worker) rather "
            "than the transient acquiring process so PID-liveness does not mark it "
            "stale instantly."
        ),
    ),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit JSON to stdout"),
    verbose: bool = typer.Option(False, "--verbose", help="More detail on stderr"),
    harness: Optional[str] = typer.Option(
        None,
        "--harness",
        help=(
            "Pin the owning harness on the claim instead of resolving it from "
            "ambient markers. The init hook passes the PROVEN harness here so a "
            "session that inherited a foreign marker does not tag its claim with "
            "the precedence winner."
        ),
    ),
    handover_from: Optional[str] = typer.Option(
        None,
        "--handover-from",
        help=(
            "Take over a claim currently held by this EXACT prior holder, "
            "instead of failing as held-by-other. For the dispatch handover: "
            "`fno agents spawn --node` claims the node before its worker "
            "exists, and the worker names that holder here to inherit it. "
            "Falls through to a normal acquire when no such claim is on disk."
        ),
    ),
) -> None:
    """Acquire a claim on KEY for HOLDER. Idempotent re-acquire if HOLDER matches.

    With ``--lane <id> --max-lanes N`` this acquires a lane SLOT instead, which
    is the same operation against a different claim space; it was a separate
    ``lane-acquire`` verb whose name was its own flag.
    """
    if lane is not None:
        if key is not None or max_lanes is None or holder:
            typer.echo(
                "validation error: --lane takes no KEY and no --holder, and "
                "requires --max-lanes (a lane slot is acquired by lane id, not "
                "by claim key)",
                err=True,
            )
            raise typer.Exit(code=2)
        _acquire_lane(lane=lane, max_lanes=max_lanes, ttl=ttl, json_output=json_output)
        return
    # --max-lanes is meaningless without --lane, and silently ignoring it is how
    # someone migrating off `lane-acquire` gets an ordinary claim with the cap
    # not enforced and nothing on stderr to say so.
    if max_lanes is not None:
        typer.echo(
            "validation error: --max-lanes is the lane-slot cap and requires "
            "--lane <id>",
            err=True,
        )
        raise typer.Exit(code=2)
    if key is None:
        typer.echo("validation error: KEY is required (or use --lane <id>)", err=True)
        raise typer.Exit(code=2)
    if not holder:
        typer.echo("validation error: --holder is required", err=True)
        raise typer.Exit(code=2)
    # ponytail: an omitted --pid used to anchor to the TRANSIENT acquiring process
    # (a one-shot `fno claim acquire` from a shell dies ~1s later, so the claim went
    # instantly STALE -- the footgun). Default instead to the durable session
    # (nearest harness ancestor: claude/codex/gemini/opencode/agy) when one
    # exists; degrade to the prior os.getpid() default when not (standalone use,
    # plain-shell, no agent session). Reuses the exact walk init-target-state.sh
    # already runs via `fno claim session-pid`.
    if pid is None:
        try:
            from .session_pid import resolve_session_pid
            pid = resolve_session_pid()
        except Exception:
            pid = None  # degrade to acquire_claim's os.getpid() default
    # The handover runs FIRST and is strictly additive: it either moves a claim
    # whose prior holder the caller named exactly, or it declines and the
    # ordinary acquire below runs unchanged. Declining covers every case that is
    # not this handover - no claim on disk, a free key, a different holder - and
    # acquire then applies its own rules, including refusing a live foreign
    # claim. So a wrong or stale --handover-from never widens what can be taken.
    if handover_from and key:
        from .core import RebindRefused, compare_and_rebind

        try:
            claim, _mode = compare_and_rebind(
                key,
                handover_from,
                new_holder=holder,
                # The claim changes OWNER, so it must stop describing the
                # spawner. The init hook passes a PROVEN harness precisely so a
                # session that inherited a foreign marker does not mislabel its
                # claim, and silently keeping the spawner's tag would defeat it.
                new_reason=reason or None,
                new_harness=harness,
                new_pid=pid,
                ttl_ms=_parse_ttl(ttl),
                root=_node_aware_root(key),
            )
        except RebindRefused:
            pass
        else:
            typer.echo(
                json.dumps(claim.model_dump(mode="json"))
                if json_output
                else f"acquired {key} (handover from {handover_from})"
            )
            return
    try:
        claim = acquire_claim(
            key=key,
            holder=holder,
            reason=reason or None,
            ttl_ms=_parse_ttl(ttl),
            metadata=_parse_metadata(metadata),
            pid=pid,
            harness=harness,
            root=_node_aware_root(key),
        )
    except ClaimValidationError as exc:
        typer.echo(f"validation error: {exc}", err=True)
        raise typer.Exit(code=2)
    except ClaimHeldByOther as exc:
        typer.echo(
            f"claim {key!r} held by {exc.holder} (pid={exc.pid}, host={exc.host})",
            err=True,
        )
        raise typer.Exit(code=1)
    except (ClaimCorrupted, ClaimGoneAway) as exc:
        typer.echo(f"transient error: {exc}", err=True)
        raise typer.Exit(code=3)
    except ClaimContended as exc:
        # acquire_claim's own contention-retry-exhaustion guard: same
        # "caller should retry later" semantic as ClaimHeldByOther, so it
        # gets the same exit code rather than an uncaught traceback.
        typer.echo(f"contention error: {exc}", err=True)
        raise typer.Exit(code=1)

    # do provenance opens at acquire - the one choke point a session killed
    # mid-phase still reaches (release/finalize fire only on a clean terminal).
    # started_at from this claim's own acquire time; ended_at stays open for the
    # release path to fill. Best-effort and node-keyed, mirroring the release
    # stamp's contract. A caller that acquires as a serialization step before
    # its own validation (init's post-claim check-contained) can still be
    # refused after this row is open, so it rolls the row back on that path via
    # `release --rollback-do` - the stamp stays here, at the one choke point
    # every acquire path reaches, rather than being deferred per caller.
    if key.startswith("node:"):
        _stamp_do_on_acquire(key, claim, holder)

    if json_output:
        typer.echo(json.dumps(claim.to_yaml_dict()))
    else:
        typer.echo(f"acquired: {key} (holder={holder}, pid={claim.pid})")


@cli.command()
def release(
    key: Optional[str] = typer.Argument(None, help="Claim key. Omit when using --lane."),
    holder: str = typer.Option("", "--holder"),
    lane: Optional[str] = typer.Option(
        None,
        "--lane",
        help=(
            "Release the LANE SLOT held by this lane id instead of a keyed "
            "claim (was `claim lane-release --lane-id`). Takes no KEY."
        ),
    ),
    force: bool = typer.Option(
        False,
        "--force",
        "-F",
        help=(
            "Administratively drop the claim regardless of owner, archiving it "
            "to .expired/ (was `claim force-release`). Requires --reason and "
            "takes no --holder."
        ),
    ),
    reason: str = typer.Option("", "--reason", "-R", help="With --force: required audit rationale."),
    strict: bool = typer.Option(False, "--strict", help="Raise if holder does not match"),
    stamp_do: bool = typer.Option(
        False, "--stamp-do",
        help="Stamp a do provenance row (started_at from this claim's acquire time, "
             "ended_at now). Set ONLY by a session releasing its OWN node claim at a "
             "finished terminal - never a handoff, which runs under a successor's "
             "identity and would mis-attribute the predecessor's window.",
    ),
    rollback_do: bool = typer.Option(
        False, "--rollback-do",
        help="Remove the open do provenance row this claim's acquire opened. Set "
             "by a releaser whose POST-ACQUIRE validation refused it: it took the "
             "claim only to serialize, did no work, and must not leave the node "
             "reading as in progress. Only an open row (no ended_at) whose "
             "started_at matches this claim is removed. Mutually exclusive with "
             "--stamp-do.",
    ),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Release a claim we own. Silent success if already released.

    Three modes that were three verbs, because two of the names were flags:
    the default releases a claim we hold, ``--lane <id>`` releases a lane slot,
    and ``--force`` drops a claim regardless of owner.
    """
    # A flag that belongs to another mode is REFUSED, never ignored. Silently
    # dropping `--stamp-do` on the force path (or `--reason` on the plain one)
    # loses exactly the provenance the flag was passed to record, and the caller
    # gets exit 0 saying it worked.
    if lane is not None:
        if key is not None or force or holder or strict or stamp_do or rollback_do or reason:
            typer.echo(
                "validation error: --lane takes only the lane id (no KEY, and "
                "none of --force/--holder/--strict/--reason/--stamp-do/"
                "--rollback-do): a lane slot has no owner and no do window",
                err=True,
            )
            raise typer.Exit(code=2)
        _release_lane(lane=lane, json_output=json_output)
        return
    if key is None:
        typer.echo("validation error: KEY is required (or use --lane <id>)", err=True)
        raise typer.Exit(code=2)
    if reason and not force:
        typer.echo(
            "validation error: --reason records the --force override and has no "
            "effect on an ordinary release",
            err=True,
        )
        raise typer.Exit(code=2)
    if force:
        if strict or stamp_do or rollback_do:
            typer.echo(
                "validation error: --force is the administrative drop and takes "
                "none of --strict/--stamp-do/--rollback-do (there is no owner to "
                "check and no do window to stamp)",
                err=True,
            )
            raise typer.Exit(code=2)
        if holder:
            typer.echo(
                "validation error: --force drops the claim regardless of owner, "
                "so --holder is meaningless with it",
                err=True,
            )
            raise typer.Exit(code=2)
        if not reason:
            typer.echo(
                "validation error: --force requires --reason (the override is "
                "recorded in the audit trail)",
                err=True,
            )
            raise typer.Exit(code=2)
        _force_release(key=key, reason=reason, json_output=json_output)
        return
    if not holder:
        typer.echo("validation error: --holder is required (or use --force)", err=True)
        raise typer.Exit(code=2)
    if stamp_do and rollback_do:
        typer.echo(
            "validation error: --stamp-do and --rollback-do are mutually "
            "exclusive (one records a finished do window, the other removes a "
            "row for work that never ran)",
            err=True,
        )
        raise typer.Exit(code=2)
    try:
        released = release_claim(
            key=key, holder=holder, strict=strict, root=_node_aware_root(key)
        )
    except HolderMismatch as exc:
        typer.echo(f"holder mismatch: {exc}", err=True)
        raise typer.Exit(code=4)
    except ClaimValidationError as exc:
        typer.echo(f"validation error: {exc}", err=True)
        raise typer.Exit(code=2)
    except (ClaimCorrupted, ClaimGoneAway) as exc:
        typer.echo(f"transient error: {exc}", err=True)
        raise typer.Exit(code=3)

    # do provenance: the third choke point (ship=pr_number, blueprint=plan_path,
    # do=claim release). started_at from the claim's own acquire time, ended_at
    # at the release instant - a true per-session hold window, not the
    # stamp-fire time. The --stamp-do gate means only a session releasing its
    # own claim (the finished-terminal path) records it.
    if released is not None and key.startswith("node:"):
        if stamp_do:
            _stamp_do_on_release(key, released, holder)
        elif rollback_do:
            _rollback_do_on_release(key, released, holder)

    if json_output:
        typer.echo(json.dumps({"key": key, "released": True}))
    else:
        typer.echo(f"released: {key}")


def _owned_do_identity(claim, holder: str) -> "tuple[str, str]":
    """Resolve the (harness, session_id) a do provenance row should be written
    under: the OWNED identity, not the ambient env.

    The harness the claim was pinned to (init passes the proven --harness;
    ``claim.harness`` carries it) and the session encoded in the holder
    (``target-session:<id>``). Ambient marker precedence would launder an
    inherited foreign marker into the row (x-0bb9), so ambient is only a
    fallback when the owned values are absent. Shared by the acquire and
    release stamps so they always agree on the row key - release must fill the
    row acquire opened, not open a second one."""
    from fno.harness_identity import resolve_harness_identity

    ident = resolve_harness_identity()
    harness = (getattr(claim, "harness", None) or ident.harness or "").strip()
    session_id = ""
    if holder and holder.startswith("target-session:"):
        session_id = holder.split(":", 1)[1].strip()
    if not session_id:
        session_id = (ident.session_id or "").strip()
    return harness, session_id


def _do_row_coordinates(key: str, claim, holder: str, action: str):
    """The ``(node_id, harness, session_id, started_at)`` naming the do row for
    this claim, or ``None`` after printing the named skip.

    All three do-row writers (open at acquire, close at release, roll back at a
    refused acquire) must address the SAME row, so they derive its coordinates
    here rather than each re-deriving them. ``started_at`` is the claim's own
    acquire time - the honest phase start, which sat in the claim file all along.
    ``action`` names the caller in the skip line."""
    node_id = key.split(":", 1)[1] if ":" in key else ""
    if not node_id:
        return None
    harness, session_id = _owned_do_identity(claim, holder)
    if not harness or not session_id:
        typer.echo(
            f"claim {action}: no owned identity for the do provenance row of "
            f"{node_id}; the row is skipped. Skipped.",
            err=True,
        )
        return None
    from datetime import datetime, timezone

    started = datetime.fromtimestamp(
        claim.acquired_at / 1000, tz=timezone.utc
    ).strftime("%Y-%m-%dT%H:%M:%SZ")
    return node_id, harness, session_id, started


def _stamp_do_on_acquire(key: str, claim, holder: str) -> None:
    """Open the do lifecycle row at claim acquire, recording started_at from the
    claim's own acquire time and leaving ended_at open for the release path to
    fill. Best-effort: a graph failure or missing identity is a named stderr
    skip and never fails the acquire.

    A session killed mid-phase never reaches its release terminal, so the
    release/finalize stamps never fire and the row would be lost entirely -
    including a started_at that sat in the claim file the whole time. Opening
    the row at acquire guarantees it exists from the moment work starts;
    append_session_record's duplicate-fill lets release close it (ended_at) and
    is a no-op on a re-acquire or a retried stamp.

    Identity is the OWNED one, not the ambient env: the harness the claim was
    pinned to (init passes the proven --harness; ``claim.harness`` carries it)
    and the session encoded in the holder (``target-session:<id>``). Ambient
    marker precedence would launder an inherited foreign marker into the row
    (x-0bb9), so it is only a fallback when the owned values are absent.

    Two reachable acquire paths both call this: the CLI ``claim acquire`` verb
    (the init-script cold start) and ``_reacquire_node_claim`` (a target-start
    takeover that calls ``acquire_claim`` in-process and bypasses this Typer
    command). Both must stamp, or a killed session on either path loses its row.

    A caller that acquires the claim as a serialization step and may still be
    refused by a post-acquire re-check owns the rollback: see ``release
    --rollback-do``.
    """
    from fno.graph.store import append_session_record
    from fno.paths import graph_json

    coords = _do_row_coordinates(key, claim, holder, "acquire")
    if coords is None:
        return
    node_id, harness, session_id, started = coords
    try:
        found, _added = append_session_record(
            graph_json(), node_id, phase="do",
            harness=harness, session_id=session_id,
            started_at=started,
        )
    except (Exception, SystemExit) as exc:
        typer.echo(
            f"claim acquire: do provenance open skipped for {node_id}: {exc}",
            err=True,
        )
        return
    # append_session_record returns (found=False, added=False) without raising
    # when the node id is absent from the graph (a superseded node whose claim
    # file lingers). The named-skip contract requires an explicit stderr line so
    # the operator knows provenance was not opened, not silently dropped.
    if not found:
        typer.echo(
            f"claim acquire: do provenance open skipped for {node_id} "
            f"(node not in graph); the row was not written. Skipped.",
            err=True,
        )


def _stamp_do_on_release(key: str, claim, holder: str) -> None:
    """Close the do lifecycle row for the session that just released a node
    claim: fills ended_at (started_at already set at acquire). Best-effort: a
    graph failure or missing identity is a named stderr skip and never fails
    the release. Uses the same owned identity as the acquire stamp
    (``_owned_do_identity``) so it fills the row acquire opened rather than
    opening a second one when ambient and owned diverge."""
    from datetime import datetime, timezone

    from fno.graph.store import append_session_record
    from fno.paths import graph_json

    coords = _do_row_coordinates(key, claim, holder, "release")
    if coords is None:
        return
    node_id, harness, session_id, started = coords
    ended = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    try:
        found, _added = append_session_record(
            graph_json(), node_id, phase="do",
            harness=harness, session_id=session_id,
            started_at=started, ended_at=ended,
        )
    except (Exception, SystemExit) as exc:
        typer.echo(
            f"claim release: do provenance stamp skipped for {node_id}: {exc}",
            err=True,
        )
        return
    # append_session_record returns (found=False, added=False) without raising
    # when the node id is absent from the graph (a superseded node whose claim
    # file lingers). The named-skip contract requires an explicit stderr line
    # so the operator knows provenance was lost, not silently dropped.
    if not found:
        typer.echo(
            f"claim release: do provenance stamp skipped for {node_id} "
            f"(node not in graph); the row was not written. Skipped.",
            err=True,
        )


def _rollback_do_on_release(key: str, claim, holder: str) -> None:
    """Drop the open do row this claim's acquire opened, for a worker whose
    post-acquire validation refused it.

    Acquiring is not doing. ``fno target init`` takes the claim purely as a
    serialization point and only then re-runs its containment gate; a worker
    refused there releases without ``--stamp-do`` because it must not proceed -
    but the acquire stamp had already opened its row, leaving the node reading
    as permanently in progress for work nobody performed.

    The removal is guarded in the graph primitive, not here: only an OPEN row
    (no ended_at) whose started_at equals this claim's acquire time is dropped,
    so a real earlier window under the same identity survives. Best-effort and
    named on skip, matching the two stamps - a rollback failure must not turn a
    refusal into a crash."""
    from fno.graph.store import remove_open_session_record
    from fno.paths import graph_json

    coords = _do_row_coordinates(key, claim, holder, "release --rollback-do")
    if coords is None:
        return
    node_id, harness, session_id, started = coords
    try:
        found, removed = remove_open_session_record(
            graph_json(), node_id, phase="do",
            harness=harness, session_id=session_id,
            started_at=started,
        )
    except (Exception, SystemExit) as exc:
        typer.echo(
            f"claim release: do provenance rollback skipped for {node_id}: {exc}",
            err=True,
        )
        return
    if not found:
        typer.echo(
            f"claim release: do provenance rollback skipped for {node_id} "
            f"(node not in graph); nothing was removed. Skipped.",
            err=True,
        )
    elif not removed:
        # Not an error: the acquire stamp itself may have been skipped (no owned
        # identity, node absent at the time), or the row is a closed window this
        # rollback must not touch. Say which outcome happened rather than let
        # silence read as "the open row was removed".
        typer.echo(
            f"claim release: no open do row to roll back for {node_id} "
            f"(none was opened, or the row is already closed).",
            err=True,
        )


@cli.command()
def refresh(
    key: str = typer.Argument(...),
    holder: str = typer.Option(..., "--holder"),
    ttl: str = typer.Option("", "--ttl"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Extend a TTL claim's expires_at. No-op for PID-liveness claims."""
    try:
        result = refresh_claim(key=key, holder=holder, ttl_ms=_parse_ttl(ttl), root=_node_aware_root(key))
    except HolderMismatch as exc:
        typer.echo(f"holder mismatch: {exc}", err=True)
        raise typer.Exit(code=4)
    except ClaimGoneAway as exc:
        typer.echo(f"claim missing: {exc}", err=True)
        raise typer.Exit(code=3)
    except ClaimValidationError as exc:
        typer.echo(f"validation error: {exc}", err=True)
        raise typer.Exit(code=2)
    except ClaimCorrupted as exc:
        typer.echo(f"corrupted claim: {exc}", err=True)
        raise typer.Exit(code=3)
    except ClaimContended as exc:
        # refresh_claim's own contention-retry-exhaustion guard; same exit
        # code as acquire's, both mean "transient, caller should retry".
        typer.echo(f"contention error: {exc}", err=True)
        raise typer.Exit(code=1)

    if result is None:
        if json_output:
            typer.echo(json.dumps({"key": key, "refreshed": False, "reason": "pid_liveness"}))
        else:
            typer.echo(f"no-op for PID-liveness claim: {key}")
        return

    if json_output:
        typer.echo(json.dumps(result.to_yaml_dict()))
    else:
        typer.echo(f"refreshed: {key} (new expires_at={result.expires_at})")


#: States in which nobody holds the key, so a reader is about to conclude the
#: node is free for the taking. ``suspect`` is absent on purpose: a dead pid
#: inside its TTL still protects the slot and already reads as held.
_UNHELD_STATES = frozenset({"free", "stale"})


class RosterReading(NamedTuple):
    """One reading of the fleet roster, reusable across many claims.

    ``consulted`` is the honest instrument flag: False means the join could not
    run, and ``reason`` says why. ``rows_scanned`` is the positive marker - a
    scan of forty rows finding nobody is a different answer from a read that
    failed, and rendering both the same way is how this defect would survive its
    own fix. ``workers_by_node`` maps a resolved node id to the rows on it, and
    ``rows_by_session`` maps a session id to its own row.

    Taken ONCE and passed down. The read shells out to the harness, so a sweep
    over sixty claims that took its own reading each time would pay sixty
    subprocesses to answer one question.

    WHAT THIS READING CANNOT SEE, and it decides how the probe below is allowed
    to use it. ``fleet_rows`` enumerates ``claude agents --json --all`` and drops
    ``kind == "interactive"``. So a codex or opencode worker, and any
    hand-started session, has NO row here by construction. An empty
    ``workers_on`` therefore means "not in this reading", never "nobody is
    working that node".
    """

    consulted: bool
    rows_scanned: int
    workers_by_node: dict
    reason: str = ""
    rows_by_session: dict = {}

    def workers_on(self, node_id: str) -> list:
        return self.workers_by_node.get(node_id, [])

    def row_for_session(self, session_id: str):
        return self.rows_by_session.get(session_id)


def read_roster(timeout: float = 10.0) -> RosterReading:
    """Read the fleet once and index it by resolved node id.

    The join is :func:`fno.agents.watchdog.fleet_rows`, which resolves a row's
    node from the worktree manifest and then the session-keyed ledger - both
    machine-written. Never a name regex: eight auto-named workers read as
    nobody-on-this-node on 2026-08-15 and were nearly double-dispatched, which
    is recorded in that module's header.

    Distinguishing "scanned and found nothing" from "could not scan" is the
    whole point, so the degrade signal is explicit rather than inferred from an
    empty index. ``fleet_rows`` returns ``([], warnings)`` for every failure
    mode, so no rows PLUS a warning is an instrument that did not run, while no
    rows and no warning is an honestly empty fleet.

    A row whose node did not resolve carries ``node=None`` and is invisible
    here - exactly the shape of a worker that never ran ``target init``. The
    scanned count is the honest ceiling on what was checked, which is why every
    caller prints it rather than just the hits.
    """
    try:
        from fno.agents.watchdog import fleet_rows

        rows, warnings = fleet_rows(timeout=timeout)
    except Exception as exc:  # noqa: BLE001 - any failure must degrade loudly
        return RosterReading(False, 0, {}, f"{type(exc).__name__}: {exc}")
    if warnings:
        # ANY warning means the enumeration is incomplete, not just an empty
        # one. fleet_rows warns on dropped session-id-less rows and on a roster
        # probe approaching its budget, and both return a PARTIAL list. Treating
        # a truncated scan as authoritative is the same absence-as-evidence
        # move this cross-check exists to delete, one layer up.
        return RosterReading(False, 0, {}, warnings[0])
    index: dict = {}
    by_session: dict = {}
    for r in rows:
        entry = {"name": r.name, "state": r.state, "cwd": r.cwd}
        if r.node:
            index.setdefault(r.node, []).append(entry)
        if r.row_id:
            by_session[str(r.row_id)] = entry
    return RosterReading(True, len(rows), index, "", by_session)


def _roster_crosscheck(node_id: str, reading: Optional[RosterReading] = None) -> dict:
    """The additive ``roster_*`` fields for one node key.

    ``reading`` lets a sweep share one fleet read across every claim it
    classifies; omitting it takes a fresh reading for a single lookup.
    """
    if reading is None:
        reading = read_roster(timeout=_ROSTER_CROSSCHECK_TIMEOUT_S)
    if not reading.consulted:
        return {
            "roster_consulted": False,
            "roster_rows_scanned": 0,
            "roster_workers": [],
            "roster_skip_reason": reading.reason,
        }
    return {
        "roster_consulted": True,
        "roster_rows_scanned": reading.rows_scanned,
        "roster_workers": reading.workers_on(node_id),
    }


#: A roster row in this state finished its turn. The session is RESUMABLE, so
#: the row is worth naming, but nobody is driving the node right now and
#: calling it a live worker would be the same overclaim the cross-check exists
#: to delete - one word covering two facts.
_FINISHED_ROW_STATE = "done"


def _roster_verdict_line(info: dict) -> str:
    """One line naming what was consulted and what it found.

    Each string is produced by exactly one outcome, so a caller asserts a
    positive marker instead of grepping for the absence of the word free - an
    absence has two explanations and cannot tell them apart, which is the
    defect this whole cross-check exists to remove.

    Four outcomes, not three: a node whose only roster rows are finished
    sessions is genuinely unworked, and printing the live-worker alarm for it
    would train every reader to ignore the alarm.
    """
    if not info.get("roster_consulted"):
        return f"free, roster not consulted ({info.get('roster_skip_reason', 'unknown')})"
    workers = info.get("roster_workers") or []
    engaged = [w for w in workers if w["state"] != _FINISHED_ROW_STATE]
    if engaged:
        rendered = ", ".join(f"{w['name']} (state={w['state']})" for w in engaged)
        return f"UNCLAIMED but a live worker is on this node: {rendered}"
    scanned = f"free, no live worker found (roster scanned: {info['roster_rows_scanned']} rows)"
    if workers:
        rendered = ", ".join(w["name"] for w in workers)
        return f"{scanned}; {len(workers)} finished session(s) resolved to it: {rendered}"
    return scanned


@cli.command()
def status(
    key: str = typer.Argument(...),
    json_output: bool = typer.Option(False, "--json", "-J"),
    roster: bool = typer.Option(
        True,
        "--roster/--no-roster",
        help=(
            "Cross-check the fleet roster before calling a node: key free "
            "(default on). --no-roster is for a hot-path caller that only "
            "reads the claim record, e.g. the claim heartbeat."
        ),
    ),
) -> None:
    """Inspect a single claim. Exit code reflects state for scripting.

    On a ``node:<id>`` key that nobody holds, the roster is cross-checked
    before the answer is rendered. ``free`` had two explanations the output
    could not tell apart - nobody is working this, and the claim never got
    taken - and four kings read the first while getting the second, then
    staffed a duplicate onto a node that already had a live worker on it.

    Only ``node:`` keys, because only they have a fleet to cross-check
    against. Every other key renders exactly as before.

    Callers parsing the JSON keep reading ``state``; the roster fields are
    additive and only appear when the cross-check applies.

    ``--no-roster`` exists for one shape of caller: a hot path that reads only
    the claim record. ``hooks/claim-heartbeat.sh`` runs on tool calls and reads
    ``.holder`` alone, and the cross-check's whole cost lands on exactly the
    branch it hits when a claim has lapsed. Paying a harness subprocess there to
    compute a field it discards would tax every tool call to answer a question
    it never asked.
    """
    info = claim_status(key=key, root=_node_aware_root(key))
    node_id = key[len("node:"):] if key.startswith("node:") else ""
    crosschecked = roster and bool(node_id) and info.get("state") in _UNHELD_STATES
    if crosschecked:
        info.update(_roster_crosscheck(node_id))
    if json_output:
        typer.echo(json.dumps(info))
        return
    typer.echo(json.dumps(info, indent=2))
    if crosschecked:
        typer.echo(_roster_verdict_line(info))


def _merge_claims_across_roots(
    deduped_roots: list[tuple[Optional[Path], Path]],
    *,
    prefix: str,
    include_stale: bool,
) -> tuple[list[dict], dict[str, str], dict[str, int]]:
    """Merge per-root claim listings into one best-state-wins view.

    Returns ``(all_rows, row_roots, totals)``. A key present in more than one
    root (a bug elsewhere writing to the wrong root, or a deliberate
    FNO_CLAIMS_ROOT migration leaving stale claims under the old root - see
    the version-skew note on CLAIMS_ROOT_ENV in io.py) is one logical claim,
    so every purpose (row, root attribution, totals bucket) must agree on
    ONE winning sighting rather than splitting across roots. That winner is
    the most informative state seen (live beats suspect beats stale beats
    corrupted beats free) - a first-scanned root's stale leftover from an
    old FNO_CLAIMS_ROOT migration must not hide a live claim a later root
    holds for the same key, which pure first-root-wins did. First root is
    only the tiebreak between two EQUAL-priority sightings of the same key.
    """
    _STATE_PRIORITY = {"live": 0, "suspect": 1, "stale": 2, "corrupted": 3, "free": 4}
    best_state: dict[str, str] = {}
    best_root: dict[str, str] = {}
    best_row: dict[str, Optional[dict]] = {}

    for candidate_root, cdir in deduped_roots:
        rows, _counts, states_by_key = list_claims_with_counts(
            prefix=prefix or None, include_stale=include_stale, root=candidate_root,
        )
        row_by_key = {r["key"]: r for r in rows}
        for key, state in states_by_key.items():
            priority = _STATE_PRIORITY.get(state, len(_STATE_PRIORITY))
            current = best_state.get(key)
            # Both sides default to worst-priority (never best): an unrecognized
            # `current` must stay displaceable by any recognized state, not
            # freeze the winner the way defaulting to 0 (best) would.
            if current is not None and priority >= _STATE_PRIORITY.get(
                current, len(_STATE_PRIORITY)
            ):
                continue
            best_state[key] = state
            best_root[key] = str(cdir)
            best_row[key] = row_by_key.get(key)

    all_rows = [row for row in best_row.values() if row is not None]
    all_rows.sort(key=lambda r: r["key"])
    row_roots = {key: root for key, root in best_root.items() if best_row.get(key) is not None}
    # Only stale/corrupted/free ever feed the empty-store message below;
    # live/suspect/total would just be dead weight summed on every list call.
    totals = {"stale": 0, "corrupted": 0, "free": 0}
    for state in best_state.values():
        if state in totals:
            totals[state] += 1
    return all_rows, row_roots, totals


@cli.command(name="list")
def list_cmd(
    prefix: str = typer.Option("", "--prefix", help="Filter keys starting with this prefix"),
    include_stale: bool = typer.Option(False, "--include-stale"),
    json_output: bool = typer.Option(False, "--json", "-J"),
    root: Optional[Path] = typer.Option(
        None, "--root", help="Explicit claims root (repo root); overrides the both-roots default"
    ),
) -> None:
    """Enumerate claims under the claims directory.

    Both claims roots (global ~/.fno/claims and the cwd-local root) are
    always read and merged in one run, --prefix or not - a bare `fno
    claim list` used to resolve only whichever single root an empty
    prefix happened to fall to, silently missing the other store (measured:
    574 lockfiles in a root a bare `list` could never reach).
    A colon-less or unrecognized --prefix cannot tell which root its keys
    live in (:func:`fno.claims.io.claims_root_for` returns None for
    exactly that case), so narrowing to a single guessed root would
    silently reintroduce the same miss; only an explicit --root narrows.
    """
    if root is not None:
        roots: list[Optional[Path]] = [root]
    else:
        # _node_aware_root("") already resolves to None via claims_root_for's
        # own colon check, so no separate `if prefix` branch is needed here.
        roots = [global_claims_root(), _node_aware_root(prefix)]

    deduped_roots = dedup_claims_roots(roots)
    all_rows, row_roots, totals = _merge_claims_across_roots(
        deduped_roots, prefix=prefix, include_stale=include_stale
    )
    n_roots = len(deduped_roots)

    if json_output:
        # Bare list, matching the original shape: a scripted caller already
        # does `for r in json.loads(...)`. The filtered-count fix below is a
        # human-output problem only - JSON already answers unambiguously
        # (an empty list here really does mean zero rows in this mode).
        typer.echo(json.dumps(all_rows))
        return

    if not all_rows:
        # AC7: a store that is mostly stale must never print the bare
        # string "no claims" - that reads identically to an empty store.
        # The --include-stale hint only makes sense when the flag wasn't
        # already given and there is actually something it would surface -
        # otherwise it tells the caller to pass a flag they already passed.
        hint = ""
        if not include_stale and (totals["stale"] or totals["corrupted"]):
            hint = "; --include-stale to list them"
        typer.echo(
            f"no live claims ({totals['stale']} stale, {totals['corrupted']} corrupted, "
            f"{totals['free']} released mid-scan across "
            f"{n_roots} root{'s' if n_roots != 1 else ''}){hint}"
        )
        return
    for r in all_rows:
        typer.echo(
            f"{r['state']:9} {r['key']:32} holder={r.get('holder', '-')} "
            f"pid={r.get('pid', '-')} host={r.get('host', '-')} root={row_roots[r['key']]}"
        )


#: Holder prefix marking a claim taken by `fno agents spawn --node` on behalf of
#: a worker that does not exist yet. Worker-specific (it carries the worker's
#: name), so naming it back is evidence of being the intended successor rather
#: than a bystander who guessed the key.
HANDOVER_HOLDER_PREFIX = "spawn-handover:"


#: Roster row states that mean the session finished its turn. A row in any
#: other state is still in play. ``done`` is NOT terminal for the session (it is
#: resumable), but it IS the positive marker that this worker stopped working,
#: which is the only thing abandonment needs to know.
_TERMINAL_ROW_STATES = frozenset({"done"})


def _abandonment_probe(reading: Optional[RosterReading] = None):
    """The roster-backed probe :func:`reap_dead_claims` calls on a SUSPECT node.

    Returns a callable answering ``True`` (proven abandoned), ``False`` (the
    holder is still working) or ``None`` (unproven, so the claim is kept). One
    roster READING is taken lazily and shared across every claim in the sweep.

    ABANDONMENT IS PROVEN BY FINDING THE HOLDER, NEVER BY FAILING TO FIND IT.
    The probe resolves the claim's own holder session id and requires its roster
    row to exist and read terminal. An absent row answers ``None``.

    That asymmetry is the whole safety argument, and an earlier version of this
    function got it wrong in the exact way the third pitfalls entry describes.
    It asked "does any row resolve to this node", read the empty answer as
    abandonment, and defended it with a scanned-row count. But a row count
    validates the INSTRUMENT, never the TARGET. ``fleet_rows`` enumerates
    ``claude agents --json --all`` and drops interactive rows, so a codex worker,
    an opencode worker, and any hand-started session are invisible to it BY
    CONSTRUCTION. A forty-row scan that cannot represent the holder at all would
    have read as forty rows of proof, and reaping on it archives a live worker's
    claim - x-ba4b's disaster from the other side, which the
    ``reaped_a_live_worker`` kill criterion exists to stop.

    So the roster's coverage gap now costs a missed reap rather than a wrongly
    archived claim. That is the correct direction to fail: an unreaped claim
    expires on its own TTL, and a wrongly reaped one hands a live worker's node
    to a second worker.
    """
    cache: dict = {}

    def _probe(claim) -> Optional[bool]:
        if claim.holder.startswith(HANDOVER_HOLDER_PREFIX):
            # A launch window, not an abandoned session. Between spawn and
            # `target init` the worker has no worktree manifest and no ledger
            # row, so the roster cannot resolve it to the node BY CONSTRUCTION.
            # Nothing is stranded by declining: the handover claim is TTL-bound,
            # and an expired claim is provably dead on its own.
            return None
        session_id = _holder_session_id(claim.holder)
        if not session_id:
            # A holder shape this lane cannot parse names no session to look up.
            return None
        if "reading" not in cache:
            cache["reading"] = (
                reading
                if reading is not None
                else read_roster(timeout=_ROSTER_CROSSCHECK_TIMEOUT_S)
            )
        seen: RosterReading = cache["reading"]
        if not seen.consulted:
            return None
        row = seen.row_for_session(session_id)
        if row is None:
            # Not found is not gone. See the docstring.
            return None
        return row.get("state") in _TERMINAL_ROW_STATES

    return _probe


def _holder_session_id(holder: str) -> Optional[str]:
    """The session id inside a claim holder, via the canonical parser.

    One holder vocabulary, owned by ``fno.agents.truth_status``. A foreign
    holder shape returns None and condemns nothing.
    """
    try:
        from fno.agents.truth_status import _session_from_holder

        return _session_from_holder(holder)
    except Exception:  # noqa: BLE001 - an unparseable holder proves nothing
        return None


@cli.command(name="reap")
def reap_cmd(
    apply: bool = typer.Option(
        False, "--apply", help="Archive dead claims. Default is dry-run: report what would be reaped."
    ),
    root: Optional[List[Path]] = typer.Option(
        None,
        "--root",
        help="Repeatable. Overrides the default both-roots sweep "
        "(global ~/.fno/claims + the cwd-local root).",
    ),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Archive every provably-dead claim, naming the instrument that proved it.

    Not an age cutoff - death is measured, never guessed from how old a file is.
    Which measurement applies depends on what the claim's holder is:

      * An EXPIRED TTL is a clock reading and proves death from any host.
      * A dead PID proves death only on this machine, by machine_id.
      * A dead one-shot `dispatch:` holder is provable death even inside its
        TTL: nothing respawns under `spawn-cli:<pid>`.
      * A SUSPECT `node:` claim is the one case a pid cannot settle, because a
        session can be respawned under a new pid. It is reaped only on a
        POSITIVE roster finding: the join ran, scanned at least one row, and no
        row resolves to that node. A join that could not run yields unknown, and
        unknown keeps the claim.

    Dry-run by default; `--apply` archives to `.expired/` and re-reads the store
    to confirm each move before counting it `reaped` - an exit code alone is not
    evidence. Exits 1 when any reapable file's move could not be confirmed.
    """
    summary = reap_dead_claims(
        roots=list(root) if root else None,
        apply=apply,
        abandonment_probe=_abandonment_probe(),
    )

    if json_output:
        typer.echo(json.dumps(summary))
    else:
        for path, reason in summary["reap_failed"]:
            typer.echo(f"FAILED  {path}  ({reason})", err=True)
        if apply:
            typer.echo(f"reaped {summary['reaped']} of {summary['scanned']} scanned")
        else:
            typer.echo(
                f"would reap {summary['would_reap']} of {summary['scanned']} scanned "
                "(dry-run; pass --apply)"
            )
        # The suspect buckets are split because "kept: 2 suspect" is the line
        # that taught the operator this verb was useless: it could not say
        # whether those two were protected by a measurement or merely unmeasured.
        suspect = f"{summary['kept_suspect']} suspect"
        if summary["kept_suspect_alive"]:
            suspect += f", {summary['kept_suspect_alive']} suspect (worker alive)"
        if summary["kept_suspect_unprobed"]:
            suspect += (
                f", {summary['kept_suspect_unprobed']} suspect (roster not consulted)"
            )
        typer.echo(
            f"kept: {summary['kept_live']} live, {suspect}, "
            f"{summary['kept_offhost']} off-host, {summary['corrupted']} corrupted, "
            f"{summary['vanished']} vanished, {summary['contended']} contended  |  "
            f"roots: {', '.join(summary['roots'])}"
        )

    if summary["reap_failed"]:
        raise typer.Exit(code=1)


@cli.command(name="session-pid")
def session_pid(
    from_pid: Optional[int] = typer.Option(
        None,
        "--from-pid",
        help="Start the ancestor walk here (default: this process's parent).",
    ),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Resolve the durable session pid (nearest harness ancestor:
    claude/codex/gemini/opencode/agy) for the hybrid liveness pid-arm. Prints the
    pid on stdout, or nothing when uncapturable (plain-shell / no harness
    ancestor; the caller degrades to TTL-only liveness). Always exit 0 - a
    missing pid is a safe degrade, not an error (ab-cc5553f2)."""
    from .session_pid import resolve_session_pid

    pid = resolve_session_pid(from_pid=from_pid)
    if json_output:
        typer.echo(json.dumps({"session_pid": pid}))
    elif pid is not None:
        typer.echo(str(pid))
    # else: emit nothing on stdout so `$(fno claim session-pid)` is empty.


def _acquire_lane(*, lane: str, max_lanes: int, ttl: str, json_output: bool) -> None:
    """The former `claim lane-acquire`, now the --lane mode of `claim acquire`.

    Exit 1 when the cap is full (no free slot) - the same "retry later" code as
    a held claim. The cap is enforced by atomic slot acquisition, never a count.
    """
    from .lanes import acquire_lane_slot

    try:
        claim = acquire_lane_slot(
            max_lanes=max_lanes, lane_id=lane, ttl_ms=_parse_ttl(ttl or "1h")
        )
    except ClaimValidationError as exc:
        typer.echo(f"validation error: {exc}", err=True)
        raise typer.Exit(code=2)

    if claim is None:
        typer.echo(f"lane cap full (max_lanes={max_lanes})", err=True)
        raise typer.Exit(code=1)

    if json_output:
        out = claim.to_yaml_dict()
        out["lane_id"] = lane
        typer.echo(json.dumps(out))
    else:
        typer.echo(f"acquired lane slot {claim.key} for lane {lane}")


def _release_lane(*, lane: str, json_output: bool) -> None:
    """The former `claim lane-release`. Silent success if the lane holds none."""
    from .lanes import release_lane_slot

    release_lane_slot(lane_id=lane)
    if json_output:
        typer.echo(json.dumps({"lane_id": lane, "released": True}))
    else:
        typer.echo(f"released lane {lane}")


def _force_release(*, key: str, reason: str, json_output: bool) -> None:
    """The former `claim force-release`. Archived to .expired/."""
    try:
        force_release_claim(key=key, reason=reason, root=_node_aware_root(key))
    except ClaimValidationError as exc:
        typer.echo(f"validation error: {exc}", err=True)
        raise typer.Exit(code=2)

    if json_output:
        typer.echo(json.dumps({"key": key, "force_released": True, "reason": reason}))
    else:
        typer.echo(f"force-released: {key}")


__all__ = ["cli"]
