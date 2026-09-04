"""``fno agents king`` - board reads and session init for the king loop."""
from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Optional, cast

import typer

king_app = typer.Typer(
    name="king",
    help="The king's board: what still needs doing, and the session manifest for its loop.",
    no_args_is_help=True,
)


def _default_max_rows() -> int:
    from fno.king.board import DEFAULT_MAX_ROWS

    return DEFAULT_MAX_ROWS


EVENTS_PATH = ".fno/events.jsonl"


def _emit_cancel_signal(path: Path, scope: str) -> None:
    """Record a king cancel after the sentinel is safely on disk."""
    try:
        from fno.events import _build, append_event

        append_event(
            _build(
                "cancel_signal_set",
                "agents",
                {"lane": "king", "path": str(path), "scope": scope, "reason": "operator"},
            ),
            events_path=path.parent.parent / "events.jsonl",
        )
    except Exception as exc:  # noqa: BLE001 - the sentinel remains authoritative
        typer.echo(f"king: WARNING: cancel signal event not emitted: {exc}", err=True)


@king_app.command("init")
def init_cmd(
    scope: str = typer.Option(..., "--scope", help="What this king was crowned over."),
    harness_session_id: str = typer.Option(
        "", "--harness-session-id", help="The king's own harness session id."
    ),
    max_iterations: int = typer.Option(
        40, "--max-iterations", help="Iteration ceiling before the loop stops on Budget."
    ),
    respawn_ceiling: int = typer.Option(
        4,
        "--respawn-ceiling",
        help="King sessions the walk may respawn before it terminates on Budget.",
    ),
    force: bool = typer.Option(
        False, "--force", "-F", help="Replace an existing manifest."
    ),
) -> None:
    """Write this crown scope's manifest, which the king loop arms read.

    Write-once, like the target manifest. Without it the stop hook allows exit
    silently, which is the correct posture for a session nobody crowned. An
    ended king's manifest is expired by `fno agents king done`, so a successor
    init needs --force only when the predecessor died without abdicating.
    """
    from fno.king.state import (
        KingManifestExists,
        king_loop_enabled,
        king_manifest_path,
        write_manifest,
    )

    # The ONE chokepoint for `config.king.enabled`. Every arm - this hook shim,
    # `loop-check --driver king`, and `KingQueue` - arms on the manifest's
    # existence, so gating the manifest gates all three at one place. Gating
    # them individually is the corpus's "guard on one of N reachable paths",
    # and the version this replaces had N of zero: the flag was read only by
    # `fno agents autonomy status`, so a default-off king still held sessions open.
    if not king_loop_enabled():
        typer.echo(
            "king: config.king.enabled is false, so no king is crowned. "
            "Enable it with `fno config set king.enabled true`.",
            err=True,
        )
        raise typer.Exit(3)

    # An id-less manifest is the same defect from the other side: the hook can
    # match nobody against it, so it either gates every session or none. Refuse
    # to write one rather than ship a crown that cannot be attributed.
    if not harness_session_id.strip():
        typer.echo(
            "king: --harness-session-id is required. The stop hook gates the "
            "session the manifest NAMES, so an unattributable manifest crowns "
            "nobody and risks holding unrelated sessions open.",
            err=True,
        )
        raise typer.Exit(2)

    try:
        manifest_path = king_manifest_path(scope)
        fields = write_manifest(
            manifest_path,
            scope=scope,
            harness_session_id=harness_session_id,
            max_iterations=max_iterations,
            respawn_ceiling=respawn_ceiling,
            force=force,
        )
        manifest_path.with_suffix(".cancelled").unlink(missing_ok=True)
    except KingManifestExists as exc:
        typer.echo(str(exc), err=True)
        raise typer.Exit(1) from exc
    typer.echo(f"king: manifest written: {manifest_path}")
    typer.echo(f"fno_id: {fields['fno_id']}")
    typer.echo(f"scope:  {fields['scope']}")
    _warn_uncrowned_row(scope)


def _warn_uncrowned_row(scope: str) -> None:
    """Say out loud when the manifest is armed but the row carries no crown.

    Arming and authority are separate on purpose: `king init` writes the
    manifest and never stamps the row, because a session that could stamp its
    own crown could crown itself. `fno agents crown` owns that write and needs
    an attended shell or a superior crown.

    The split is correct and the SILENCE about it is not. Three readers -
    `king done`, `king manifest-path`, and `hooks/king-postcompact-reinject.sh`
    - all key on the row's crown fields, and all three fail CLOSED and QUIETLY
    when it is absent: done refuses, manifest-path exits non-zero and prints
    no path, and the post-compact brief never arrives. Absent covers a crown
    over OTHER territory too - the readers key on an exact crown_scope, so a
    containing crown is as unusable to them as none at all. A king can hold a valid manifest all
    evening and never learn its authority is invisible to the machine.

    So this warns, and never refuses: the manifest is written and the loop arms
    on the FILE, which works. Warning-only also keeps a legitimate arm-then-
    crown ordering usable.

    Three outcomes, kept distinct because an absence has more than one cause.
    A resolved row with no crown is the real gap. An unresolvable row is an
    unanswered question, not a finding, and says so rather than claiming the
    crown is missing.
    """
    from fno.agents.crown import (
        AGENT_UNREGISTERED,
        REGISTRY_UNREADABLE,
        calling_agent_row,
        crown_scope_matches,
        crown_reading,
    )

    try:
        row = calling_agent_row()
    except Exception:  # noqa: BLE001 - a warning never breaks a written manifest
        return
    if row is REGISTRY_UNREADABLE or row is AGENT_UNREGISTERED or row is None:
        typer.echo(
            "king: warning: manifest armed, but this session resolves to no "
            "registry row, so its crown cannot be checked. Run `/fno-me` to "
            "register, then have an attended shell run "
            f"`fno agents crown <handle> --scope {scope}`.",
            err=True,
        )
        return
    handle = getattr(row, "name", "") or "<handle>"
    reading = crown_reading(row)
    if reading is not None and crown_scope_matches(reading.get("scope"), scope):
        return

    # What the three row-keyed readers do when the crown does not cover the
    # scope just armed. Identical for a missing crown and a mismatched one:
    # each keys on crown_scope, so a crown over other territory is as absent
    # to them as no crown at all.
    consequence = (
        "`fno agents king done` will refuse, `fno agents king manifest-path` "
        "will exit non-zero without printing a path so the stop hook leaves "
        "KING_STATE_FILE unset, and the post-compact king brief will never "
        "arrive."
    )
    if reading is None:
        typer.echo(
            f"king: warning: manifest armed for {scope!r}, but this row "
            f"carries NO crown, so {consequence} Ask an attended shell to run "
            f"`fno agents crown {handle} --scope {scope}`.",
            err=True,
        )
        return
    typer.echo(
        f"king: warning: manifest armed for {scope!r}, but this row's crown is "
        f"{reading['label']!r}, which is not that scope. These readers key on "
        f"an EXACT crown_scope, so a crown over wider territory does not "
        f"satisfy them any more than a crown over unrelated territory: "
        f"{consequence} Ask an attended shell to re-scope it with "
        f"`fno agents crown {handle} --scope {scope}`.",
        err=True,
    )


@king_app.command("done")
def done_cmd(
    scope: str = typer.Option(
        "", "--scope", help="Crown scope to expire. Default: this session's own crown."
    ),
) -> None:
    """Expire this crown: vacate the row and clear the scope manifest.

    The abdication half of the crown lifecycle. A king ending its reign
    calls it, so a successor's crown arms without --force. A king that dies
    without calling it leaves an inert manifest: the registry row is
    authority, so a leftover file captures nobody.
    """
    from dataclasses import replace as _replace

    from fno.agents.crown import (
        AGENT_UNREGISTERED,
        REGISTRY_UNREADABLE,
        calling_agent_row,
    )
    from fno.agents.registry import TERMINAL_STATUSES as _TERMINAL_ROW_STATUSES
    from fno.agents.registry import update_registry
    from fno.king.state import king_manifest_path, parse_manifest, remove_king_manifest

    caller = calling_agent_row()
    if caller is REGISTRY_UNREADABLE or caller is AGENT_UNREGISTERED:
        typer.echo(
            "king: cannot verify the caller's crown: this session carries an "
            "agent identity the registry does not resolve to a row, and "
            "expiring a crown requires a holder. Run /fno-me or retry, or "
            "pass --scope from an attended shell.",
            err=True,
        )
        raise typer.Exit(2)

    if caller is None:
        if not scope.strip():
            typer.echo(
                "king: an attended shell holds no crown of its own; pass "
                "--scope <territory> to expire that crown.",
                err=True,
            )
            raise typer.Exit(2)
        # A named scope may still have a LIVE king reigning over it; expiring
        # only the manifest would disarm that king's stop-hook floor while its
        # row still reads crowned. Resolve the holder inside the vacate
        # closure below (under the registry lock) and vacate it too; a scope
        # with no live holder is the orphan-cleanup case and clears the file
        # alone.
        holder_name = ""
    else:
        own = getattr(caller, "crown_scope", None)
        if not scope.strip():
            if not own:
                typer.echo(
                    "king: this session holds no crown, so there is nothing "
                    "to expire.",
                    err=True,
                )
                raise typer.Exit(2)
            scope = own
        elif own != scope:
            typer.echo(
                f"king: refusing to expire {scope!r}: this session's crown is "
                f"{own!r}, and an agent expires only its own crown. Call "
                "`fno agents king done` with no --scope, or use an attended "
                "shell for another territory.",
                err=True,
            )
            raise typer.Exit(2)
        holder_name = caller.name

    # Snapshot the manifest's OWN session id BEFORE vacating: the removal
    # below compares against it under the manifest lock, so a successor
    # crowned in the vacate window (which writes its own id into the same
    # scope file) survives instead of having its manifest unlinked. The id is
    # read from the file, never the caller's row, so a resumed king whose row
    # id has moved on still expires the manifest it was armed with.
    try:
        expired_manifest_session = (
            parse_manifest(king_manifest_path(scope)).get("harness_session_id") or None
        )
    except (OSError, ValueError):
        expired_manifest_session = None

    # Vacate the row BEFORE touching the file, under the registry lock: the
    # vacate closure re-reads the row's crown, so a scope that moved to a
    # successor mid-call is refused here instead of disarming the successor's
    # manifest below. Same order the succession path uses (stamp the heir,
    # then clean the vacated file).
    vacated = holder_name is None
    if holder_name is not None:
        attended_named = holder_name == ""

        def _vacate(rows: list) -> list:
            nonlocal vacated
            for index, row in enumerate(rows):
                if attended_named:
                    # An attended shell naming a scope: vacate whatever live
                    # row still holds it, or clear nothing when the scope is
                    # already orphaned (the manifest-only cleanup below).
                    if (
                        row.crown_scope == scope
                        and row.status not in _TERMINAL_ROW_STATUSES
                    ):
                        rows[index] = _replace(
                            row,
                            crown_level=None,
                            crown_scope=None,
                            crown_grantor=None,
                        )
                        vacated = True
                elif row.name == holder_name and row.crown_scope == scope:
                    rows[index] = _replace(
                        row, crown_level=None, crown_scope=None, crown_grantor=None
                    )
                    vacated = True
                    break
            return rows

        try:
            update_registry(_vacate)
        except Exception as exc:  # noqa: BLE001 - named, never swallowed
            typer.echo(f"king: crown expire failed: {exc}", err=True)
            raise typer.Exit(1) from exc
        if not vacated and not attended_named:
            typer.echo(
                f"king: refusing to expire {scope!r}: this row no longer holds "
                "it (the crown moved or was already vacated), so the manifest "
                "on disk may belong to a successor. Re-read with "
                "`fno agents court` before expiring anything.",
                err=True,
            )
            raise typer.Exit(1)

    # expected_harness_session_id is the snapshot taken above, compared under
    # the manifest lock: it is the successor-race guard, not an ownership
    # proof (the locked vacate above already proved that). A False return
    # here means the file on disk is no longer the manifest this expiry
    # targeted - most likely a successor's fresh one.
    if not remove_king_manifest(
        scope, expected_harness_session_id=expired_manifest_session
    ):
        typer.echo(
            f"king: row vacated, but the manifest for {scope!r} could not be "
            "removed: the file no longer names the session this expiry "
            "snapshotted, so a successor crowned mid-expiry likely owns it "
            "now. Re-read with `fno agents court` before touching anything.",
            err=True,
        )
        raise typer.Exit(1)
    typer.echo(f"king: crown expired: {scope}")
    typer.echo(
        "row crown: vacated; manifest: cleared"
        if vacated
        else "row crown: no live holder found; manifest: cleared"
    )


@king_app.command("cancel")
def cancel_cmd(
    scope: str = typer.Option(..., "--scope", help="Crown scope whose walk to cancel."),
    clear: bool = typer.Option(False, "--clear", help="Clear the king cancel signal."),
) -> None:
    """Set or clear the cancel signal beside a canonical crown manifest."""
    from fno.king.state import king_manifest_path

    manifest = king_manifest_path(scope)
    if not manifest.is_file():
        typer.echo(
            f"king: refusing cancel for {scope!r}; no king manifest at canonical path "
            f"{manifest}",
            err=True,
        )
        raise typer.Exit(1)
    sentinel = manifest.with_suffix(".cancelled")
    try:
        if clear:
            sentinel.unlink(missing_ok=True)
            typer.echo(f"king: cancel signal cleared: {sentinel}")
        else:
            sentinel.touch()
            _emit_cancel_signal(sentinel, scope)
            typer.echo(f"king: cancel signal set: {sentinel}")
    except OSError as exc:
        typer.echo(f"king: could not update cancel signal {sentinel}: {exc}", err=True)
        raise typer.Exit(1) from exc


@king_app.command("shape")
def shape_cmd(
    shape: str = typer.Argument(..., help="pass or court."),
    scope: str = typer.Option(
        "", "--scope", help="Crown scope to reshape. Default: this session's own crown."
    ),
) -> None:
    """Declare this reign's shape: a pure pass, or a court holding workers.

    The field the Stop nudge reads. An undeclared court - a reign that spawned
    workers and left the manifest at ``pass`` - is nagged at every stop,
    because choosing court had no machine-visible act before this verb. Declare
    ``court`` the moment the reign spawns its first worker.
    """
    from fno.agents.crown import (
        AGENT_UNREGISTERED,
        REGISTRY_UNREADABLE,
        calling_agent_row,
    )
    from fno.king.state import set_manifest_shape

    if shape not in ("pass", "court"):
        typer.echo("king: shape must be 'pass' or 'court'.", err=True)
        raise typer.Exit(2)

    caller = calling_agent_row()
    if caller is REGISTRY_UNREADABLE or caller is AGENT_UNREGISTERED:
        typer.echo(
            "king: cannot resolve the caller's crown: this session carries an "
            "agent identity the registry does not resolve to a row. Run /fno-me "
            "or retry.",
            err=True,
        )
        raise typer.Exit(2)
    if caller is None:
        typer.echo(
            "king: an attended shell holds no crown; the crowned session "
            "declares its own shape from inside the reign.",
            err=True,
        )
        raise typer.Exit(2)
    own = getattr(caller, "crown_scope", None)
    if not own:
        typer.echo(
            "king: this session holds no crown, so there is no reign to shape.",
            err=True,
        )
        raise typer.Exit(2)
    if scope.strip() and scope != own:
        typer.echo(
            f"king: refusing to reshape {scope!r}: this session's crown is "
            f"{own!r}, and a holder declares only its own reign's shape.",
            err=True,
        )
        raise typer.Exit(2)
    session_id = (
        getattr(caller, "harness_session_id", None)
        or getattr(caller, "cc_session_id", None)
        or ""
    )
    try:
        new_value = set_manifest_shape(own, shape, expect_session_id=session_id or None)
    except ValueError as exc:
        typer.echo(f"king: {exc}", err=True)
        raise typer.Exit(1) from exc
    typer.echo(f"king: shape declared: {new_value}")
    typer.echo(f"scope:  {own}")


@king_app.command("manifest-path", hidden=True)
def manifest_path_cmd(
    harness_session_id: str = typer.Option(..., "--harness-session-id"),
    harness: str = typer.Option("", "--harness"),
    state_root: Optional[Path] = typer.Option(None, "--state-root"),
) -> None:
    """Print this live crowned session's existing scope manifest path."""
    from fno.king.state import resolve_king_manifest_path

    path = resolve_king_manifest_path(
        harness_session_id,
        harness or None,
        state_root=state_root,
    )
    if path is None:
        raise typer.Exit(1)
    typer.echo(path)


@king_app.command("board")
def board_cmd(
    as_json: bool = typer.Option(False, "--json", "-J", help="Emit the board payload."),
    max_rows: int = typer.Option(
        _default_max_rows(), "--max-rows", help="Rows rendered per queue."
    ),
    last_run: bool = typer.Option(
        False,
        "--last-run",
        help="Instead of reading the board, ask whether a king walk terminated recently.",
    ),
    since: str = typer.Option("24h", "--since", help="Window for --last-run (e.g. 24h, 90m, 7d)."),
    state: Optional[Path] = typer.Option(
        None, "--state", hidden=True, help="King manifest whose scope bounds the board."
    ),
) -> None:
    """Report every queue that would keep a king working.

    Exits non-zero when any queue could not be read: an unreadable queue is not
    an empty one, and a reader who could not tell them apart would call a broken
    verb a clean board.
    """
    from fno.king.board import read_board

    if last_run:
        from fno.king.state import last_run_is_fresh, parse_window

        try:
            window_s = parse_window(since)
        except ValueError as exc:
            typer.echo(str(exc), err=True)
            raise typer.Exit(2) from exc
        fresh = last_run_is_fresh(Path(EVENTS_PATH), since_s=window_s)
        typer.echo(f"last king walk within {since}: {'yes' if fresh else 'no'}")
        raise typer.Exit(0 if fresh else 1)

    scope = None
    if state is not None:
        from fno.king.state import parse_manifest

        scope = parse_manifest(state).get("scope")
        if not scope:
            board = {
                "actionable": 1,
                "unreadable": 1,
                "queues": [
                    {
                        "name": "scope",
                        "source": str(state),
                        "status": "unreadable",
                        "error": "king manifest has no scope",
                        "count": None,
                        "rows": [],
                        "actionable": True,
                        "note": "",
                    }
                ],
                "warnings": [],
                "exit_code": 1,
            }
        else:
            board = read_board(scope=scope)
    else:
        board = read_board()
    if as_json:
        typer.echo(json.dumps(board, indent=2))
    else:
        _render(board, max_rows)
    raise typer.Exit(cast(int, board["exit_code"]))


@king_app.command("escalate")
def escalate_cmd(
    stalled: str = typer.Option(
        "", "--stalled", help="Comma-separated board rows nothing is clearing."
    ),
    reason: str = typer.Option(
        "NoProgress", "--reason", "-R", help="The terminal reason that triggered this."
    ),
) -> None:
    """Tell the operator the king stopped with work still pending.

    Called by BOTH king terminals - the stop hook's NoProgress and the walk
    arm's per-unit park - because a guard on one of two reachable paths is
    decorative. Idempotent per stalled id set, so a respawned king meeting the
    same stalled board never records a second question.
    """
    from fno.carveout.core import resolve_carveout_root, resolve_session_id
    from fno.king.escalate import escalate
    from fno.king.state import reign_state
    from fno.paths import resolve_repo_root

    ids = [part.strip() for part in stalled.split(",") if part.strip()]
    try:
        session_id = resolve_session_id(resolve_repo_root())
    except Exception:  # noqa: BLE001 - an unresolvable session never blocks the ask
        session_id = None
    # The caller's own liveness, read not asserted: a live king that reads
    # "It has exited" in its own escalation is handed "crown a new king" as a
    # remedy - the double-crown failure court exists to end. Unknown reads as
    # dead inside question_text, with the reason named.
    try:
        state = reign_state(session_id=session_id)
        live, unknown_reason = state.live, state.unknown_reason
    except Exception as exc:  # noqa: BLE001 - escalation must still fire
        live, unknown_reason = None, f"reign_state unreadable: {exc}"
    try:
        outcome, qid = escalate(
            ids,
            reason=reason,
            root=resolve_carveout_root(),
            session_id=session_id,
            cwd=Path.cwd(),
            live=live,
            unknown_reason=unknown_reason,
        )
    except Exception as exc:  # noqa: BLE001 - named, never swallowed
        typer.echo(f"king: escalation failed: {exc}", err=True)
        raise typer.Exit(1) from exc
    typer.echo(f"king: {outcome} {qid}", err=True)
    typer.echo(qid)


def _render(board: dict, max_rows: int) -> None:
    typer.echo(f"actionable: {board['actionable']}")
    for q in board["queues"]:
        if q["status"] == "unreadable":
            typer.echo(f"  {q['name']:<20} UNREADABLE  {q['error']}")
        else:
            mark = "*" if q["actionable"] and q["count"] else " "
            verb = f"  -> {q['verb']}" if q.get("verb") else ""
            note = f"  ({q['note']})" if q["note"] and q["count"] else ""
            typer.echo(f" {mark}{q['name']:<20} {q['count']}{verb}{note}")
            for row in q["rows"][:max_rows]:
                typer.echo(f"      {row}")
            hidden = max(0, len(q["rows"]) - max_rows)
            if hidden:
                typer.echo(f"      ... {hidden} more not shown")
        typer.echo(f"      source: {q['source']}")
    for warning in board["warnings"]:
        typer.echo(f"warning: {warning}", err=True)


agents_king_app = typer.Typer(
    name="king",
    help="The king session manifest and escalation controls.",
    no_args_is_help=True,
)
agents_king_app.command("init")(init_cmd)
agents_king_app.command("done")(done_cmd)
agents_king_app.command("cancel")(cancel_cmd)
agents_king_app.command("escalate")(escalate_cmd)
agents_king_app.command("shape")(shape_cmd)
# The stop hooks resolve the crown manifest here. They once reached it
# through the deprecated `fno king` spelling that verb_moves forwards onto
# THIS app; the verb missed the fold, so the resolver exited 2 and every
# stop on an active kings dir burned its unavailable-retries before
# allowing exit. The hooks now name `agents king` directly, so the
# deprecation clock cannot re-open that hole. Hidden: a hook surface, not
# menu UI.
agents_king_app.command("manifest-path", hidden=True)(manifest_path_cmd)


def main() -> None:  # pragma: no cover - console-script shim
    sys.exit(king_app())
