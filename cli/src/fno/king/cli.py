"""``fno king`` - board reads and session init for the king loop."""
from __future__ import annotations

import json
import sys

import typer

king_app = typer.Typer(
    name="king",
    help="The king's board: what still needs doing, and the session manifest for its loop.",
    no_args_is_help=True,
)


@king_app.command("board")
def board_cmd(
    as_json: bool = typer.Option(False, "--json", "-J", help="Emit the board payload."),
    max_rows: int = typer.Option(25, "--max-rows", help="Rows rendered per queue."),
) -> None:
    """Report every queue that would keep a king working.

    Exits non-zero when any queue could not be read: an unreadable queue is not
    an empty one, and a reader who could not tell them apart would call a broken
    verb a clean board.
    """
    from fno.king.board import read_board

    board = read_board(max_rows=max_rows)
    if as_json:
        typer.echo(json.dumps(board, indent=2))
    else:
        _render(board)
    raise typer.Exit(board["exit_code"])


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
