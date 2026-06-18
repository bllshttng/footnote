"""fno megawalk - front door for the Rust loop walker.

The Python walker machinery (run_walker, megawalk_host, megawalk-stop-hook)
was deleted in task 2.4 of control-plane step 5 (ab-7303e5d7). The walk
is now driven by:

    fno-agents loop run --driver megawalk [flags]

This module retains:
  - The Typer app registration (cli.py LAZY_SUBCOMMANDS "megawalk" expects it)
  - A callback/stub pointing users at the new front door
  - The `watch` subcommand (live TUI; unchanged)

Subcommands that referenced walker-session internals (pause, resume, bootstrap,
reset, status) are removed along with the walker. The /megawalk skill covers
status via `fno backlog` + journal tail.
"""
from __future__ import annotations

from pathlib import Path
from typing import Optional

import typer

app = typer.Typer(
    name="megawalk",
    help="Iterate the fno backlog. Use `fno-agents loop run --driver megawalk` to start the walk.",
    no_args_is_help=False,
)

_NEW_FRONT_DOOR = (
    "The Python megawalk walker is superseded by the Rust loop runtime "
    "(control-plane step 5, ab-7303e5d7). To start a walk:\n\n"
    "    fno-agents loop run --driver megawalk [--project NAME] [--all] "
    "[--allow-merge] [--max-units N]\n\n"
    "Or use the /megawalk skill in Claude Code for an interactive launch-and-watch session.\n"
    "For the live TUI: fno megawalk watch"
)


@app.callback(invoke_without_command=True)
def main(ctx: typer.Context) -> None:
    """Iterate the fno backlog until done, blocked, or budget cap hit."""
    if ctx.invoked_subcommand is None:
        typer.echo(_NEW_FRONT_DOOR, err=True)
        raise typer.Exit(code=12)


@app.command(name="watch")
def watch_cmd(
    state_dir: Path = typer.Option(
        Path(".fno"),
        "--state-dir",
        "-D",
        help="Directory containing events.jsonl (canonical) or megawalk-events.jsonl (legacy).",
    ),
) -> None:
    """Live TUI showing in-flight units, recent events, and walk status."""
    from fno.megawalk_tui import watch as run_watch
    # Repointed: canonical events.jsonl (loop source "loop" events).
    events_path = state_dir / "events.jsonl"
    raise typer.Exit(code=run_watch(events_path))
