"""`fno outstanding` - read what is waiting on a human; ask and clear questions.

Machine-first, mirroring `fno carveout`: stdout carries the value (the report,
or a new question id), guidance and warnings go to stderr, and exit codes are
predictable (0 ok / 1 read or write failure).
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import typer

from fno.outstanding.core import OutstandingError, collect, render

outstanding_app = typer.Typer(
    help=(
        "What is outstanding FOR YOU: unharvested carve-outs and open operator "
        "questions. Read-only by default; `ask` records a question so it "
        "survives the next turn, `clear` closes it once answered."
    ),
)


def _storage_root() -> Path:
    """The canonical root that owns both stores (shared project state)."""
    from fno.carveout.core import resolve_carveout_root

    return resolve_carveout_root()


def _session_id() -> "str | None":
    from fno.carveout.core import resolve_session_id
    from fno.paths import resolve_repo_root

    try:
        return resolve_session_id(resolve_repo_root())
    except Exception:  # noqa: BLE001 - an unresolvable session is not an error here
        return None


def _unattended() -> bool:
    """True for a spawned/bg worker.

    The hook fires in every session, so the discriminator has to be
    attendedness rather than question ownership: the OPERATOR owns none of
    these questions (workers asked them), so gating the render on ownership
    would show the one person who can answer exactly nothing.
    """
    return bool(os.environ.get("FNO_AGENT_SELF") or os.environ.get("FNO_BG"))


@outstanding_app.callback(invoke_without_command=True)
def report(
    ctx: typer.Context,
    as_json: bool = typer.Option(
        False,
        "--json",
        "-J",
        help="Emit one JSON object carrying both legs instead of the human block.",
    ),
) -> None:
    """Report unharvested carve-outs and open operator questions."""
    if ctx.invoked_subcommand is not None:
        return

    root = _storage_root()
    try:
        outstanding = collect(root)
    except OutstandingError as exc:
        # A present-but-unreadable store is a FAILED read, never "nothing
        # outstanding": reporting an empty queue here would tell the operator
        # the pile is clear when it is merely unreadable.
        typer.echo(f"outstanding: failed to read: {exc}", err=True)
        raise typer.Exit(1)

    if as_json:
        typer.echo(json.dumps(outstanding.as_dict(), separators=(",", ":")))
        return

    block = render(
        outstanding, session_id=_session_id(), unattended=_unattended()
    )
    if block:
        typer.echo(block, nl=False)
