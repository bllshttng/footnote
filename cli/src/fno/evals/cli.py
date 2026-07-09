"""fno evals subcommand - golden-task bank runner + research-brief grader.

``fno evals run``
    Execute bank tasks (``evals/bank/*.yaml``) in disposable worktrees via the
    headless spawn substrate, grade mechanically, and append one history row
    per task-run. ``--repeat K`` scores pass^k reliability.
``fno evals report``
    Fold the history: per-tier pass rates, per-task pass@1 / pass^k, flake list,
    and a regression alarm. ``--graduate`` lists saturated capability tasks.
``fno evals graduate <id>``
    Retag a capability task's YAML to regression (a reviewed edit).
``fno evals grade``
    Grade a research brief against a golden doc (three mechanical assertions).
    Backs the kept research surfaces (``/ship doc``, ``/review research``).
"""
from __future__ import annotations

from pathlib import Path
from typing import Optional

import typer


def _resolve_bank_dir(bank: Optional[Path]) -> Path:
    """Resolve the bank dir: explicit --bank, else <repo-root>/evals/bank."""
    if bank is not None:
        return bank
    from fno.paths import resolve_canonical_repo_root

    try:
        root = resolve_canonical_repo_root()
    except Exception:  # noqa: BLE001 - outside a repo, fall back to cwd
        root = Path.cwd()
    return root / "evals" / "bank"


evals_app = typer.Typer(
    name="evals",
    help="Golden-task bank (run / report / graduate) + research-brief grading (grade).",
    no_args_is_help=True,
)


@evals_app.callback()
def _evals_callback() -> None:
    """Research-brief grading.

    A no-op group callback so Typer keeps ``evals`` as a command group with
    a single subcommand instead of collapsing ``grade`` into the top-level
    callback (which would break ``fno evals grade`` routing).
    """


@evals_app.command("run")
def run_command(
    task: Optional[str] = typer.Option(None, "--task", help="Run only this task id."),
    tier: Optional[str] = typer.Option(None, "--tier", help="Run only this tier (capability|regression)."),
    repeat: int = typer.Option(1, "--repeat", "-k", help="Run each task K times (pass^k)."),
    bank: Optional[Path] = typer.Option(None, "--bank", help="Bank dir (default: <repo>/evals/bank)."),
    provider: Optional[str] = typer.Option(None, "--provider", help="Worker provider for the headless spawn."),
    yes: bool = typer.Option(False, "--yes", "-y", help="Skip the confirmation prompt above 20 total runs."),
) -> None:
    """Run bank tasks in disposable worktrees and grade them mechanically.

    Exit codes:
      0  all runs graded (individual fails are in the summary + history)
      1  no bank present, or --task/--tier selected nothing
      2  a bank task is invalid (load-time discipline violation)
    """
    from fno.evals.bank import BankError, discover_bank
    from fno.evals.runner import run_task, sweep_orphans
    from fno.paths import resolve_canonical_repo_root

    if repeat < 1:
        typer.echo("Error: --repeat must be >= 1", err=True)
        raise typer.Exit(code=1)

    bank_dir = _resolve_bank_dir(bank)
    try:
        tasks = discover_bank(bank_dir)
    except BankError as exc:
        # A missing dir means no bank; any other load error is a discipline
        # violation the author must fix.
        if "not found" in str(exc):
            typer.echo(f"Error: no bank at {bank_dir} (expected evals/bank/*.yaml).", err=True)
            raise typer.Exit(code=1)
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=2)

    if task is not None:
        tasks = [t for t in tasks if t.id == task]
    if tier is not None:
        tasks = [t for t in tasks if t.tier == tier]
    if not tasks:
        typer.echo("Error: selection matched no bank tasks.", err=True)
        raise typer.Exit(code=1)

    total_runs = len(tasks) * repeat
    if total_runs > 20 and not yes:
        typer.echo(f"About to run {len(tasks)} task(s) x {repeat} = {total_runs} live runs.")
        if not typer.confirm("Proceed?"):
            typer.echo("Aborted.")
            raise typer.Exit(code=0)

    try:
        repo_root = resolve_canonical_repo_root()
    except Exception:  # noqa: BLE001
        repo_root = Path.cwd()

    swept = sweep_orphans(repo_root)
    if swept:
        typer.echo(f"swept {swept} orphaned eval worktree(s) from a prior run")

    all_passed = True
    for t in tasks:
        results = run_task(t, repeat=repeat, repo_root=repo_root, worker_provider=provider)
        passes = sum(1 for r in results if r.passed)
        passk = "PASS" if passes == repeat else "FAIL"
        typer.echo(f"  {t.tier:11} {t.id}: {passes}/{repeat} pass  (pass^{repeat}={passk})")
        for r in results:
            if not r.passed:
                typer.echo(f"      run {r.repeat_index}: {r.reason}")
        if passes < repeat:
            all_passed = False
    typer.echo("done." if all_passed else "done (some runs failed; see history).")
    raise typer.Exit(code=0)


@evals_app.command("report")
def report_command(
    since: Optional[int] = typer.Option(None, "--since", help="Fold only the most recent N runs."),
    graduate: bool = typer.Option(False, "--graduate", help="List capability tasks eligible to graduate."),
    n: int = typer.Option(3, "--consecutive", help="Consecutive passes required for graduation eligibility."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit the report as JSON."),
    history_file: Optional[Path] = typer.Option(None, "--history", help="History file (default: paths.evals_history())."),
) -> None:
    """Fold evals history: per-tier pass rates, pass@1, pass^k, flakes, alarm.

    Exit codes:
      0  report rendered (or no data)
      4  regression alarm: a regression-tier task is below 100%
    """
    import json as _json

    from fno.evals.report import build_report, graduation_candidates, load_rows

    if history_file is None:
        from fno.paths import evals_history
        history_file = evals_history()

    rows = load_rows(history_file, since=since)
    report = build_report(rows)

    if graduate:
        candidates = graduation_candidates(rows, n=n)
        report["graduation_eligible"] = candidates

    if json_output:
        typer.echo(_json.dumps(report, indent=2))
    elif report["no_data"]:
        typer.echo("evals report: no_data (no history yet)")
    else:
        typer.echo("Evals report:")
        for tier, agg in report["tiers"].items():
            typer.echo(f"  {tier}: {agg['passes']}/{agg['runs']} pass ({agg['pass_rate']:.0%})")
        for t in report["tasks"]:
            mark = "FLAKE" if t["flake"] else ("PASS" if t["pass_k"] else "FAIL")
            typer.echo(
                f"    {t['tier']:11} {t['task_id']}: pass@1={t['pass_at_1']:.0%} "
                f"pass^{t['runs']}={t['pass_k']} [{mark}]"
            )
        if report["flakes"]:
            typer.echo(f"  flakes: {', '.join(report['flakes'])}")
        if report["regression_alarm"]:
            typer.echo(f"  REGRESSION ALARM: {', '.join(report['regression_alarm'])} below 100%")
        if graduate:
            elig = report.get("graduation_eligible") or []
            typer.echo(
                f"  graduation-eligible: {', '.join(elig)}" if elig
                else "  graduation-eligible: none"
            )

    raise typer.Exit(code=4 if report["regression_alarm"] else 0)


@evals_app.command("graduate")
def graduate_command(
    task_id: str = typer.Argument(..., help="Bank task id to graduate to the regression tier."),
    bank: Optional[Path] = typer.Option(None, "--bank", help="Bank dir (default: <repo>/evals/bank)."),
) -> None:
    """Retag a capability task's YAML tier to regression (a reviewed edit).

    Exit codes:
      0  retagged
      1  task id not found in the bank
      2  task is not capability-tier (nothing to graduate)
    """
    from fno.evals.bank import BankError, discover_bank
    from fno.evals.report import GraduateError, graduate_task_file

    bank_dir = _resolve_bank_dir(bank)
    try:
        tasks = discover_bank(bank_dir)
    except BankError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=1)

    match = next((t for t in tasks if t.id == task_id), None)
    if match is None or match.source_path is None:
        typer.echo(f"Error: no bank task '{task_id}' in {bank_dir}", err=True)
        raise typer.Exit(code=1)

    try:
        graduate_task_file(match.source_path)
    except GraduateError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=2)

    typer.echo(f"graduated '{task_id}' -> regression ({match.source_path})")
    raise typer.Exit(code=0)


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
