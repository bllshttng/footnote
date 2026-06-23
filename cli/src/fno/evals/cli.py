"""fno evals subcommand - golden-task efficacy eval runner.

Commands
--------
``fno evals run``
    Run one or all golden eval tasks, write results to evals-history.jsonl,
    and print a summary table.
``fno evals report``
    Show latest result per task plus trend, with a staleness warning when
    the suite has not run recently.
``fno evals diff``
    Show per-task assertion flips, termination changes, and cost deltas
    between two labeled sweeps (the ablation surface).

The golden fixtures live at ``evals/golden/<slug>/`` relative to the repo root.
"""
from __future__ import annotations

import os
import sys
import warnings
from pathlib import Path
from typing import List, Optional

import typer

from fno.evals.runner import run_tasks, RunnerError, _find_repo_root


evals_app = typer.Typer(
    name="evals",
    help="Golden-task efficacy evals (run / report / diff)",
    no_args_is_help=True,
)


def _golden_dir() -> Path:
    """Resolve the evals/golden/ directory from the repo root."""
    repo_root = _find_repo_root(Path.cwd())
    return repo_root / "evals" / "golden"


@evals_app.command("run")
def run_command(
    task: Optional[str] = typer.Option(
        None,
        "--task",
        help="Run only the fixture with this slug (default: all fixtures).",
    ),
    label: str = typer.Option(
        "",
        "--label",
        help="Freeform label stored in every history row (e.g. branch name or PR number).",
    ),
    model: str = typer.Option(
        "claude-sonnet-4-5",
        "--model",
        help="Model name passed to the loop script.",
    ),
    keep_workdir: bool = typer.Option(
        False,
        "--keep-workdir",
        help="Keep temporary workdirs after the run (default: remove on success).",
    ),
) -> None:
    """Run golden eval tasks and append results to evals-history.jsonl.

    Each task is run in a fresh temporary workdir.  The workdir is removed
    on success; kept (and its path printed) on any failure or when
    ``--keep-workdir`` is passed.

    Exit codes:
      0  All tasks passed all assertions
      1  One or more tasks failed or a harness error occurred
      2  Unrecoverable setup error (missing loop script, unknown task slug)
    """
    fixtures_dir = _golden_dir()
    if not fixtures_dir.is_dir():
        typer.echo(f"Error: golden fixtures directory not found: {fixtures_dir}", err=True)
        raise typer.Exit(code=2)

    loop_script_path_env = os.environ.get("FNO_EVALS_LOOP_SCRIPT")
    loop_script: Optional[Path] = Path(loop_script_path_env) if loop_script_path_env else None

    try:
        rc = run_tasks(
            fixtures_dir=fixtures_dir,
            task_slug=task,
            label=label,
            model=model,
            keep_workdir=keep_workdir,
            loop_script=loop_script,
        )
    except RunnerError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=2)
    except KeyboardInterrupt:
        typer.echo("\nInterrupted.", err=True)
        raise typer.Exit(code=1)

    raise typer.Exit(code=rc)


@evals_app.command("grade")
def grade_command(
    brief: Path = typer.Option(..., "--brief", help="Path to the research brief <slug>.md."),
    golden: Path = typer.Option(..., "--golden", help="Path to the golden discovery-*.md doc."),
    sidecar: Optional[Path] = typer.Option(
        None, "--sidecar",
        help="Path to the sources.jsonl (default: <brief-stem>.sources.jsonl beside the brief).",
    ),
) -> None:
    """Grade a research brief against a golden doc (US5, three mechanical assertions).

    Green only if: (a) zero uncited claims, (b) zero dead source URLs,
    (c) >=1 golden checklist item per section. No model in the gate; the
    research-verify panel is advisory and never changes this verdict.

    Exit codes:
      0  GREEN (all three pass)
      1  RED (one or more assertions failed)
      2  scorer setup error (missing brief / golden / sidecar)
    """
    from fno.evals.research_grade import GradeError, grade

    try:
        result = grade(brief, golden, sidecar_path=sidecar)
    except GradeError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=2)

    typer.echo(result.summary())
    raise typer.Exit(code=0 if result.green else 1)


@evals_app.command("report")
def report_command(
    task: Optional[str] = typer.Option(
        None,
        "--task",
        help="Show only the fixture with this slug (default: all fixtures).",
    ),
) -> None:
    """Show latest result per task plus trend, with a staleness warning.

    Exit codes:
      0  Report produced (or empty history - explicit message printed)
    """
    from fno import paths as _paths
    from fno.config import load_settings
    from fno.evals.history import iter_rows_tolerant
    from fno.evals.reporting import render_report

    history_path = _paths.evals_history()

    # Load staleness_days from config
    try:
        settings = load_settings()
        staleness_days = settings.config.evals.staleness_days
    except Exception:
        staleness_days = 14

    # Tolerant read - collect rows and print warnings for corrupt lines
    rows: list[dict] = []
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        for _lineno, row in iter_rows_tolerant(history_path):
            rows.append(row)

    # Print any warnings about corrupt lines to stdout so CliRunner captures them
    for w in caught:
        typer.echo(str(w.message))

    output = render_report(rows, staleness_days=staleness_days, task_filter=task)
    typer.echo(output, nl=False)


@evals_app.command("diff")
def diff_command(
    label: List[str] = typer.Option(
        ...,
        "--label",
        help="Labels to compare (must pass exactly two).",
    ),
) -> None:
    """Show per-task assertion flips, termination changes, and cost deltas.

    Pass exactly two --label values: the before and after sweep labels.

    Exit codes:
      0  Comparison produced (regressions do not fail the diff - it reports only)
      1  No comparison possible (missing label, no common tasks)
    """
    from fno import paths as _paths
    from fno.evals.history import iter_rows_tolerant
    from fno.evals.reporting import render_diff

    if len(label) != 2:
        typer.echo(
            f"Error: --label must be passed exactly twice (got {len(label)}). "
            "Usage: fno evals diff --label before --label after",
            err=True,
        )
        raise typer.Exit(code=1)

    label_a, label_b = label[0], label[1]
    history_path = _paths.evals_history()

    # Tolerant read
    rows: list[dict] = []
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        for _lineno, row in iter_rows_tolerant(history_path):
            rows.append(row)

    # Print any warnings about corrupt lines
    for w in caught:
        typer.echo(str(w.message))

    text, exit_code = render_diff(rows, label_a=label_a, label_b=label_b)
    typer.echo(text, nl=False)
    raise typer.Exit(code=exit_code)
