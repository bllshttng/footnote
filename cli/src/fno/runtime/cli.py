"""One-release shim for the retired ``fno runtime`` root (unit 6, x-9d6c).

``runtime worktree --action create`` duplicated ``worktree ensure`` and had
zero lifetime calls, so the capability folded into ``fno workspace worktree``
rather than being carried across. ``register-worker`` lives on as
``fno workspace register-worker``. This module keeps BOTH old subcommands
serving with their exact flags and behavior for one release, printing the new
spellings to stderr (stdout stays machine-parsed); the follow-up release
removes the spellings.
"""
from __future__ import annotations

import json
from pathlib import Path
from typing import Optional

import typer
from fno.tombstones import tombstone_group_cls

cli = typer.Typer(
    name="runtime",
    help=(
        "Deprecated shim: runtime is now `fno workspace` (worktree lifecycle, "
        "worker registration). This spelling is removed next release."
    ),
    no_args_is_help=True,
    cls=tombstone_group_cls("runtime"),
)

_DEPRECATION_NOTICE = (
    "fno runtime is now `fno workspace` (`fno runtime worktree --action "
    "create` -> `fno workspace worktree ensure`, `--action list` -> "
    "`fno workspace worktree status`, `--action remove` -> `fno workspace "
    "worktree archive`; `fno runtime register-worker` -> `fno workspace "
    "register-worker`). This spelling is removed next release."
)


@cli.callback()
def _runtime_callback(
    ctx: typer.Context,
    json_output: bool = typer.Option(
        False, "--json", "-J",
        help="Output structured JSON to stdout. Diagnostics go to stderr.",
    ),
) -> None:
    from fno.handoff.output import merge_json_flag
    merge_json_flag(ctx, json_output)


@cli.command(name="worktree")
def worktree_cmd(
    ctx: typer.Context,
    action: str = typer.Option(..., "--action", help="create | list | remove"),
    name: Optional[str] = typer.Option(None, "--name", help="worktree name"),
    base: str = typer.Option("main", "--base", help="base ref for create (default: main)"),
    prune_branch: bool = typer.Option(False, "--prune-branch", help="delete branch on remove"),
    json_flag: bool = typer.Option(False, "--json", "-J", help="output JSON"),
) -> None:
    """Manage git worktrees under ~/.fno/worktrees/{proj}-{name}/ (deprecated shim)."""
    typer.echo(_DEPRECATION_NOTICE, err=True)
    from fno.runtime.worktree import create_worktree, list_worktrees, remove_worktree

    try:
        if action == "create":
            if not name:
                typer.echo(json.dumps({"error": "--name required for create"}))
                raise typer.Exit(code=1)
            result = create_worktree(name=name, base=base)
        elif action == "list":
            result = {"worktrees": list_worktrees()}  # type: ignore[assignment]
        elif action == "remove":
            if not name:
                typer.echo(json.dumps({"error": "--name required for remove"}))
                raise typer.Exit(code=1)
            result = remove_worktree(name=name, prune_branch=prune_branch)
        else:
            typer.echo(json.dumps({"error": f"unknown action {action!r}; use create|list|remove"}))
            raise typer.Exit(code=1)
    except RuntimeError as exc:
        typer.echo(json.dumps({"error": str(exc)}))
        raise typer.Exit(code=1) from exc

    typer.echo(json.dumps(result))
    raise typer.Exit(code=0)


@cli.command(name="register-worker")
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
    """Register a worker manually in the workers registry (deprecated shim)."""
    typer.echo(_DEPRECATION_NOTICE, err=True)
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
