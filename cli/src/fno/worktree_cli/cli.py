"""fno worktree - thin lifecycle wrapper.

The actual git-worktree create/remove operations happen via Claude Code's
native EnterWorktree/ExitWorktree tools (or `git worktree add` directly).
This CLI exposes the bookkeeping subset of the old git-worktrees skill:
listing active worktrees with target status, cleaning up stale ones, and
archiving (remove directory, keep branch).
"""
import subprocess
from pathlib import Path
from typing import Optional

import typer

from fno._subprocess_util import propagate_returncode
from fno.paths import resolve_repo_root

app = typer.Typer(
    name="worktree",
    help="Worktree lifecycle: status, cleanup, archive.",
    no_args_is_help=True,
)


def _run_lifecycle(*args: str) -> int:
    repo_root = Path(resolve_repo_root())
    script = repo_root / "scripts" / "lib" / "worktree-lifecycle.sh"
    if not script.exists():
        typer.echo(f"worktree-lifecycle script not found at {script}", err=True)
        return 2
    try:
        # cwd=repo_root so the lifecycle script's relative paths (notably
        # .claude/worktrees/<name>) resolve against the git root even when
        # the user invokes `fno worktree archive` from a subdirectory.
        # Without this, valid worktrees were reported as missing (Codex P2).
        result = subprocess.run(["bash", str(script), *args], cwd=str(repo_root))
    except FileNotFoundError as exc:
        typer.echo(f"failed to run worktree-lifecycle: {exc}", err=True)
        return 2
    return propagate_returncode(result.returncode)


@app.command()
def status() -> None:
    """List active worktrees with branch, last-commit age, and target status."""
    raise typer.Exit(code=_run_lifecycle("status"))


@app.command()
def cleanup(
    older_than: Optional[str] = typer.Option(
        None,
        "--older-than",
        help="Remove worktrees with no commits in N days (e.g. '7d' or '7').",
    ),
    dry_run: bool = typer.Option(False, "--dry-run", "-N", help="Show what would be removed."),
    prefix: Optional[str] = typer.Option(
        None, "--prefix", help="Restrict to worktrees whose branch starts with this prefix."
    ),
) -> None:
    """Remove stale worktrees with no active target session."""
    args = ["cleanup"]
    if older_than:
        args.extend(["--older-than", older_than])
    if dry_run:
        args.append("--dry-run")
    if prefix:
        args.extend(["--prefix", prefix])
    raise typer.Exit(code=_run_lifecycle(*args))


@app.command()
def archive(name: str = typer.Argument(..., help="Worktree branch or path to archive.")) -> None:
    """Remove the worktree directory but keep the branch."""
    raise typer.Exit(code=_run_lifecycle("archive", name))
