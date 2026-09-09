"""``fno workspace`` - lazy-mounted worktree lifecycle and worker registration; old root spellings remain one-release shims."""
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

from fno.worktree_cli import app as _worktree_app  # noqa: E402

cli.add_typer(_worktree_app, name="worktree")


@cli.command(name="reap")
def reap_state_files_cmd(apply: bool = typer.Option(False, "--apply"), json_out: bool = typer.Option(False, "--json", "-J")) -> None:
    """Age-reap expendable state files without retiring agent rows."""
    import subprocess

    from fno._subprocess_util import propagate_returncode
    from fno.rust_binary import find_dev_binary, resolve_binary

    binary = find_dev_binary() or resolve_binary()
    if binary is None:
        typer.echo("fno agents workspace reap: the fno-agents binary was not found; run `fno doctor update --rust`.", err=True)
        raise typer.Exit(code=127)

    args = [str(binary), "reap", "--state-files-only", "--apply" if apply else "--dry-run"]
    args += ["--json"] if json_out else []
    try:
        result = subprocess.run(args, check=False)
    except OSError as exc:
        typer.echo(f"fno agents workspace reap: failed to run {binary}: {exc}", err=True)
        raise typer.Exit(code=127) from exc
    raise typer.Exit(code=propagate_returncode(result.returncode))


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
