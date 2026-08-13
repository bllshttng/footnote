"""fno claim - Typer surface for the work-claim verbs.

Exit codes:
    0  success
    1  ClaimHeldByOther (caller should retry later)
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
from typing import Optional

import typer

from .core import (
    ClaimCorrupted,
    ClaimGoneAway,
    ClaimHeldByOther,
    ClaimValidationError,
    HolderMismatch,
    acquire_claim,
    claim_status,
    force_release_claim,
    list_claims,
    refresh_claim,
    release_claim,
)
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
    key: str = typer.Argument(..., help="Lock key, e.g. node:ab-1234abcd"),
    holder: str = typer.Option(..., "--holder", help="Symbolic owner string"),
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
) -> None:
    """Acquire a claim on KEY for HOLDER. Idempotent re-acquire if HOLDER matches."""
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
    key: str = typer.Argument(...),
    holder: str = typer.Option(..., "--holder"),
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
    """Release a claim we own. Silent success if already released."""
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


@cli.command()
def status(
    key: str = typer.Argument(...),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Inspect a single claim. Exit code reflects state for scripting."""
    info = claim_status(key=key, root=_node_aware_root(key))
    if json_output:
        typer.echo(json.dumps(info))
    else:
        typer.echo(json.dumps(info, indent=2))


@cli.command(name="list")
def list_cmd(
    prefix: str = typer.Option("", "--prefix", help="Filter keys starting with this prefix"),
    include_stale: bool = typer.Option(False, "--include-stale"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Enumerate claims under the claims directory."""
    results = list_claims(
        prefix=prefix or None,
        include_stale=include_stale,
        root=_node_aware_root(prefix),
    )
    if json_output:
        typer.echo(json.dumps(results))
    else:
        if not results:
            typer.echo("no claims")
            return
        for r in results:
            typer.echo(
                f"{r['state']:9} {r['key']:32} holder={r.get('holder', '-')} "
                f"pid={r.get('pid', '-')} host={r.get('host', '-')}"
            )


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


@cli.command(name="lane-acquire")
def lane_acquire(
    lane_id: str = typer.Option(..., "--lane-id", help="Unique lane identity (typically the node id)"),
    max_lanes: int = typer.Option(..., "--max-lanes", help="Concurrency cap (>=1; 1 == sequential)"),
    ttl: str = typer.Option("1h", "--ttl", help="Slot TTL; the owner refreshes while the lane is alive"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Acquire a lane slot gated on the live-claim cap (parallel mode, x-42d5).

    Exit 1 when the cap is full (no free slot) - the same "retry later" code as
    a held claim. The cap is enforced by atomic slot acquisition, never a count.
    """
    from .lanes import acquire_lane_slot

    try:
        claim = acquire_lane_slot(
            max_lanes=max_lanes, lane_id=lane_id, ttl_ms=_parse_ttl(ttl)
        )
    except ClaimValidationError as exc:
        typer.echo(f"validation error: {exc}", err=True)
        raise typer.Exit(code=2)

    if claim is None:
        typer.echo(f"lane cap full (max_lanes={max_lanes})", err=True)
        raise typer.Exit(code=1)

    if json_output:
        out = claim.to_yaml_dict()
        out["lane_id"] = lane_id
        typer.echo(json.dumps(out))
    else:
        typer.echo(f"acquired lane slot {claim.key} for lane {lane_id}")


@cli.command(name="lane-release")
def lane_release(
    lane_id: str = typer.Option(..., "--lane-id", help="Lane identity to release"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Release the lane slot held by LANE_ID. Silent success if it holds none."""
    from .lanes import release_lane_slot

    release_lane_slot(lane_id=lane_id)
    if json_output:
        typer.echo(json.dumps({"lane_id": lane_id, "released": True}))
    else:
        typer.echo(f"released lane {lane_id}")


@cli.command(name="force-release")
def force_release(
    key: str = typer.Argument(...),
    reason: str = typer.Option(..., "--reason", "-R", help="Required: audit rationale"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Administratively drop a claim regardless of owner. Archived to .expired/."""
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
