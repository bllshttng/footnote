"""``fno workspace`` - worktree lifecycle and runtime-worker registration.

Minted in unit 6 of the x-9d6c reorg from the former top-level worktree
app plus the runtime merge: ``runtime worktree --action create``
duplicated what ``worktree ensure`` already covered and had zero lifetime
calls, so the capability folded in here rather than carrying the flag-shaped
command across. The old top-level spellings stay one-release shims
(``fno.verb_moves`` for ``worktree`` and, since x-6233, ``workspace`` itself;
the runtime root is retired outright - its refusal teaches this verb).
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Optional

import typer

cli = typer.Typer(
    name="workspace",
    help="Worktree lifecycle and worker registration.",
    no_args_is_help=True,
)

# The worktree lifecycle sub-group (the former top-level worktree app,
# mounted whole): status / stranded / cleanup / archive / reapable / ensure /
# policy / overlap-record / overlaps.
from fno.worktree_cli import app as _worktree_app  # noqa: E402

cli.add_typer(_worktree_app, name="worktree")


# `register-worker` moved from the retired runtime root: its other leaf, and its
# only surviving one once the duplicated worktree command folded in above.
@cli.command(name="register-worker", hidden=True)
def register_worker_cmd(
    ctx: typer.Context,
    worker_id: str = typer.Option(..., "--id", help="unique worker ID"),
    task: str = typer.Option("", "--task", help="task description"),
    campaign: str = typer.Option("", "--campaign", help="campaign/plan identifier"),
    workers_file: Optional[Path] = typer.Option(
        None,
        "--workers-file",
        help="path to workers.jsonl (default: .fno/workers.jsonl)",
    ),
    json_flag: bool = typer.Option(False, "--json", "-J", help="output JSON"),
) -> None:
    """Register a worker manually in the workers registry (used after in-session skill dispatch)."""
    from fno.runtime.registry import register_worker

    entry = register_worker(
        worker_id=worker_id,
        task=task,
        campaign=campaign,
        workers_file=workers_file,
    )

    result = {"status": "registered", "worker_id": worker_id, "entry": entry}
    typer.echo(json.dumps(result))
    raise typer.Exit(code=0)
