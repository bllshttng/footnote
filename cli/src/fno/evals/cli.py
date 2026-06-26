"""fno evals subcommand - research-brief grader.

``fno evals grade``
    Grade a research brief against a golden doc with three mechanical
    assertions (zero uncited claims, zero dead source URLs, >=1 golden
    checklist item per section). This is the mechanical green for a
    ``/ship doc`` deliverable; the research-verify panel is advisory and
    never changes the verdict.

The golden-task efficacy harness (``run`` / ``report`` / ``diff``) was
removed in the cutlist (x-c6a1). Only the research grader survives here
because it backs the kept research surfaces (``/ship doc``, ``/review
research``); it depends only on ``fno.research.core``, not the deleted
harness.
"""
from __future__ import annotations

from pathlib import Path
from typing import Optional

import typer


evals_app = typer.Typer(
    name="evals",
    help="Research-brief grading (grade).",
    no_args_is_help=True,
)


@evals_app.callback()
def _evals_callback() -> None:
    """Research-brief grading.

    A no-op group callback so Typer keeps ``evals`` as a command group with
    a single subcommand instead of collapsing ``grade`` into the top-level
    callback (which would break ``fno evals grade`` routing).
    """


@evals_app.command("grade")
def grade_command(
    brief: Path = typer.Option(..., "--brief", help="Path to the research brief <slug>.md."),
    golden: Path = typer.Option(..., "--golden", help="Path to the golden discovery-*.md doc."),
    sidecar: Optional[Path] = typer.Option(
        None, "--sidecar",
        help="Path to the sources.jsonl (default: <brief-stem>.sources.jsonl beside the brief).",
    ),
) -> None:
    """Grade a research brief against a golden doc (three mechanical assertions).

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
