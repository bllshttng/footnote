"""``fno king`` - board reads and session init for the king loop."""
from __future__ import annotations

import json
import sys
from pathlib import Path

import typer

king_app = typer.Typer(
    name="king",
    help="The king's board: what still needs doing, and the session manifest for its loop.",
    no_args_is_help=True,
)


KING_STATE_PATH = ".fno/king-state.md"
EVENTS_PATH = ".fno/events.jsonl"


@king_app.command("init")
def init_cmd(
    scope: str = typer.Option(..., "--scope", help="What this king was crowned over."),
    harness_session_id: str = typer.Option(
        "", "--harness-session-id", help="The king's own harness session id."
    ),
    max_iterations: int = typer.Option(
        40, "--max-iterations", help="Iteration ceiling before the loop stops on Budget."
    ),
    force: bool = typer.Option(
        False, "--force", "-F", help="Replace an existing manifest."
    ),
) -> None:
    """Write ``.fno/king-state.md``, the manifest the king loop arms read.

    Write-once, like the target manifest. Without it the stop hook allows exit
    silently and the walk arm refuses to dispatch, which is the correct posture
    for a session nobody crowned.
    """
    from fno.king.state import KingManifestExists, write_manifest

    try:
        fields = write_manifest(
            Path(KING_STATE_PATH),
            scope=scope,
            harness_session_id=harness_session_id,
            max_iterations=max_iterations,
            force=force,
        )
    except KingManifestExists as exc:
        typer.echo(str(exc), err=True)
        raise typer.Exit(1) from exc
    typer.echo(f"king: manifest written: {KING_STATE_PATH}")
    typer.echo(f"fno_id: {fields['fno_id']}")
    typer.echo(f"scope:  {fields['scope']}")


@king_app.command("board")
def board_cmd(
    as_json: bool = typer.Option(False, "--json", "-J", help="Emit the board payload."),
    max_rows: int = typer.Option(25, "--max-rows", help="Rows rendered per queue."),
    last_run: bool = typer.Option(
        False,
        "--last-run",
        help="Instead of reading the board, ask whether a king walk terminated recently.",
    ),
    since: str = typer.Option("24h", "--since", help="Window for --last-run (e.g. 24h, 90m, 7d)."),
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

    board = read_board(max_rows=max_rows)
    if as_json:
        typer.echo(json.dumps(board, indent=2))
    else:
        _render(board)
    raise typer.Exit(board["exit_code"])


@king_app.command("escalate")
def escalate_cmd(
    stalled: str = typer.Option(
        "", "--stalled", help="Comma-separated board rows nothing is clearing."
    ),
    reason: str = typer.Option(
        "NoProgress", "--reason", help="The terminal reason that triggered this."
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
    from fno.paths import resolve_repo_root

    ids = [part.strip() for part in stalled.split(",") if part.strip()]
    try:
        session_id = resolve_session_id(resolve_repo_root())
    except Exception:  # noqa: BLE001 - an unresolvable session never blocks the ask
        session_id = None
    try:
        outcome, qid = escalate(
            ids,
            reason=reason,
            root=resolve_carveout_root(),
            session_id=session_id,
            cwd=Path.cwd(),
        )
    except Exception as exc:  # noqa: BLE001 - named, never swallowed
        typer.echo(f"king: escalation failed: {exc}", err=True)
        raise typer.Exit(1) from exc
    typer.echo(f"king: {outcome} {qid}", err=True)
    typer.echo(qid)


def _render(board: dict) -> None:
    typer.echo(f"actionable: {board['actionable']}")
    for q in board["queues"]:
        if q["status"] == "unreadable":
            typer.echo(f"  {q['name']:<20} UNREADABLE  {q['error']}")
        else:
            mark = "*" if q["actionable"] and q["count"] else " "
            note = f"  ({q['note']})" if q["note"] and q["count"] else ""
            typer.echo(f" {mark}{q['name']:<20} {q['count']}{note}")
            for row in q["rows"]:
                typer.echo(f"      {row}")
            if q["truncated"]:
                typer.echo(f"      ... {q['truncated']} more not shown")
        typer.echo(f"      source: {q['source']}")
    for warning in board["warnings"]:
        typer.echo(f"warning: {warning}", err=True)


def main() -> None:  # pragma: no cover - console-script shim
    sys.exit(king_app())
