"""The worked authority's wheel tombstone: the native door owns the answer.

The fleet truth (registry join, roster reading, reachability, transcript tails, session-row fold) lives in the native binary; this wheel spelling has no leg left to run, so it names the door instead of carrying a second implementation.
"""
from __future__ import annotations

import typer


def cmd_worked(
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit JSON."),
) -> None:
    """Show nodes with positively identified live workers."""
    typer.echo(
        "Error: the worked authority is served by the native door; "
        "run `fno backlog worked`.",
        err=True,
    )
    raise typer.Exit(code=2)
