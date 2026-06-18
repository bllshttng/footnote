"""fno executor CLI - three-tier executor resolution.

The locked-decision parser and surface matcher are now in-package Python
modules (``fno.executor._locked`` / ``fno.executor._surface``), the SINGLE
source of truth. They were ported byte-for-byte from the retired bash scripts
``scripts/lib/{parse-locked-executor,infer-task-executor}.sh`` (ab-58645f63);
a parity test in ``tests/unit/test_executor_parity_vs_bash.py`` pins zero
routing drift.
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path
from typing import List, Optional

import typer


executor_app = typer.Typer(
    name="executor",
    help="Executor resolution (locked-decision parser + surface inference)",
    no_args_is_help=False,
    add_completion=False,
)


def _read_locked(plan_path: Path) -> str:
    """Run the locked-decision parser over plan content; return stripped stdout.

    Invoked as ``python3 -m fno.executor._locked`` so the same posture as the
    former bash shell-out is preserved (subprocess, captured output, non-zero
    exit surfaced rather than silently falling through to tier 2 or the ``do``
    default).
    """
    text = plan_path.read_text(encoding="utf-8")
    result = subprocess.run(
        [sys.executable, "-m", "fno.executor._locked"],
        input=text,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        typer.echo(
            f"fno executor: fno.executor._locked exited "
            f"rc={result.returncode}: {result.stderr.strip()}",
            err=True,
        )
        raise typer.Exit(code=2)
    return result.stdout.strip()


def _read_inferred(task_files: List[str]) -> str:
    """Run surface inference over a file list; return stripped stdout.

    Invoked as ``python3 -m fno.executor._surface``; a non-zero exit is
    surfaced rather than silently returning empty.
    """
    text = "\n".join(task_files) + "\n"
    result = subprocess.run(
        [sys.executable, "-m", "fno.executor._surface"],
        input=text,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        typer.echo(
            f"fno executor: fno.executor._surface exited "
            f"rc={result.returncode}: {result.stderr.strip()}",
            err=True,
        )
        raise typer.Exit(code=2)
    return result.stdout.strip()


@executor_app.command(
    "resolve",
    help=(
        "Resolve the executor for a plan or task via the three-tier chain: "
        "(1) locked decision in plan, (2) surface inference from task files, "
        "(3) default 'do'."
    ),
)
def resolve(
    plan_path: Optional[Path] = typer.Option(
        None,
        "--plan-path",
        help="Path to a design doc. Reads Locked Decisions section for executor lock.",
        exists=False,  # We validate manually for custom exit code
    ),
    task_files: Optional[str] = typer.Option(
        None,
        "--task-files",
        help="Comma-separated list of task file paths for surface inference.",
    ),
    explain: bool = typer.Option(
        False,
        "--explain",
        help="Print which tier resolved the executor and the final value.",
    ),
) -> None:
    # Validate plan_path exists if given
    if plan_path is not None and not plan_path.is_file():
        typer.echo(
            f"fno executor: plan file not found: {plan_path}",
            err=True,
        )
        raise typer.Exit(code=2)

    # Tier 1: locked decision from plan
    if plan_path is not None:
        locked = _read_locked(plan_path)
        if locked in ("do", "impeccable"):
            if explain:
                typer.echo(f"tier: locked\nvalue: {locked}")
            else:
                typer.echo(locked)
            return

        # locked is empty or 'mixed' - fall through to tier 2

    # Tier 2: surface inference from file list
    files_list: List[str] = []
    if task_files:
        files_list = [f.strip() for f in task_files.split(",") if f.strip()]

    if files_list:
        inferred = _read_inferred(files_list)
        if inferred in ("do", "impeccable"):
            if explain:
                typer.echo(f"tier: inference\nvalue: {inferred}")
            else:
                typer.echo(inferred)
            return

    # Tier 3: default
    if explain:
        typer.echo("tier: default\nvalue: do")
    else:
        typer.echo("do")
