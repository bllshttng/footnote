"""``fno agents peek``: read-only peer observation; registered here so the file-budget gate keeps ``agents/cli.py`` shrinking."""
from __future__ import annotations

import sys
from typing import Optional

import typer

from fno.agents.cli import agents_app


@agents_app.command("peek", hidden=True)
def cmd_peek(
    handle: Optional[str] = typer.Argument(
        None,
        help="Peer handle (alias or bare hex short-id, as in `mail send`). Omit with --all.",
    ),
    lines: int = typer.Option(
        15, "--lines", "-n", help="Show the last N transcript records (default 15; 0 for none)."
    ),
    follow: bool = typer.Option(
        False, "--follow", "-f", help="Stream new records as the peer emits them (read-only)."
    ),
    grep: Optional[str] = typer.Option(
        None,
        "--grep",
        help=(
            "Only records carrying this token, within the --lines window "
            "(raise -n to search deeper); the stderr count line reports a "
            "zero, never bare."
        ),
    ),
    all_sessions: bool = typer.Option(
        False,
        "--all",
        "-A",
        help="Search every registry session with a transcript instead of one handle.",
    ),
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit JSON-Lines rows instead of human lines."
    ),
) -> None:
    """Observe a peer read-only.

    Resolves ``<handle>`` to a live session and tails its transcript
    (claude/codex), preferring normalized status events when present. A
    pane-substrate worker (the default substrate) has no transcript; peek
    resolves it through the registry's mux ref and reads its pane. Never
    writes anything the peer reads. Exit 13 = unknown peer, 1 = known peer
    whose substrate has no reader or whose mux pane did not answer,
    0 = observed (or "no activity yet").
    """
    from fno.agents.peek import peek
    from fno.paths import state_dir

    if all_sessions and handle is not None:
        sys.stderr.write("--all searches every session; drop the handle argument\n")
        raise typer.Exit(code=2)
    if not all_sessions and handle is None:
        sys.stderr.write("usage: fno agents peek <handle>  (or --all)\n")
        raise typer.Exit(code=2)
    if follow and (all_sessions or grep is not None):
        sys.stderr.write("--follow streams one peer; --all/--grep read a tail\n")
        raise typer.Exit(code=2)

    if lines < 0:
        sys.stderr.write(f"--lines must be >= 0 (got {lines})\n")
        raise typer.Exit(code=2)

    events_path = state_dir() / "events.jsonl"
    rc = peek(
        handle or "",
        lines=lines,
        follow=follow,
        json_out=json_out,
        grep=grep,
        all_sessions=all_sessions,
        stdout=sys.stdout,
        stderr=sys.stderr,
        events_path=events_path if events_path.exists() else None,
    )
    if rc != 0:
        raise typer.Exit(code=rc)
