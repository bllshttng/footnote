"""fno doctor evals subcommand - golden-task bank runner + research-brief grader.

``fno doctor evals run``
    Execute bank tasks (``evals/bank/*.yaml``) in disposable worktrees via the
    headless spawn substrate, grade mechanically, and append one history row
    per task-run. ``--repeat K`` scores pass^k reliability.
``fno doctor evals report``
    Fold the history: per-tier pass rates, per-task pass@1 / pass^k, flake list,
    and a regression alarm. ``--graduate`` lists saturated capability tasks.
``fno doctor evals graduate <id>``
    Retag a capability task's YAML to regression (a reviewed edit).
``fno doctor evals grade``
    Grade a research brief against a golden doc (three mechanical assertions).
    Backs the kept research surfaces (``/ship doc``, ``/review research``).
"""
from __future__ import annotations

from pathlib import Path
from typing import Optional

import typer

from fno.evals.runner import BASELINE, VARIANT_RE


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
    callback (which would break ``fno doctor evals grade`` routing).
    """


@evals_app.command("run")
def run_command(
    task: Optional[str] = typer.Option(None, "--task", help="Run only this task id."),
    tier: Optional[str] = typer.Option(None, "--tier", help="Run only this tier (capability|regression)."),
    repeat: int = typer.Option(1, "--repeat", "-k", help="Run each task K times (pass^k)."),
    bank: Optional[Path] = typer.Option(None, "--bank", help="Bank dir (default: <repo>/evals/bank)."),
    provider: Optional[str] = typer.Option(None, "--provider", help="Worker provider for the headless spawn."),
    variant: str = typer.Option(BASELINE, "--variant", help="Round name: baseline, or v<N> for a scored change."),
    ref: Optional[str] = typer.Option(None, "--ref", help="Git ref to check out for a non-baseline --variant."),
    lane: Optional[str] = typer.Option(None, "--lane", help="Resolved-inventory lane name; overrides --provider."),
    cohort: Optional[str] = typer.Option(None, "--cohort", help="Cohort id recorded on every row this run writes."),
    yes: bool = typer.Option(False, "--yes", "-y", help="Skip the confirmation prompt above 20 total runs."),
) -> None:
    """Run bank tasks in disposable worktrees and grade them mechanically.

    Exit codes:
      0  all runs graded (individual fails are in the summary + history)
      1  no bank present, --task/--tier selected nothing, or a bad --variant/--ref/--lane
      2  a bank task is invalid (load-time discipline violation)
    """
    from fno.evals.bank import BankError, LaneError, discover_bank, resolve_lane
    from fno.evals.runner import run_task, sweep_orphans
    from fno.paths import resolve_canonical_repo_root

    if repeat < 1:
        typer.echo("Error: --repeat must be >= 1", err=True)
        raise typer.Exit(code=1)

    lane_coord = None
    if lane is not None:
        try:
            lane_coord = resolve_lane(lane)
        except LaneError as exc:
            typer.echo(f"Error: {exc}", err=True)
            raise typer.Exit(code=1)

    if not VARIANT_RE.match(variant):
        typer.echo(f"Error: --variant must be 'baseline' or 'v<N>', got '{variant}'", err=True)
        raise typer.Exit(code=1)
    if (variant == BASELINE) != (ref is None):
        typer.echo("Error: --variant v<N> and --ref REF must be used together", err=True)
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

    # Headless spawn REQUIRES a provider (the Rust agents runtime rejects a
    # provider-less non-pane spawn), so a prompt-bearing task with no --provider
    # would grade every run as a spawn failure. Default to claude when any
    # selected task has a prompt. Grade-only tasks never spawn, so they are fine.
    if provider is None and lane_coord is None and any(t.prompt for t in tasks):
        provider = "claude"
        typer.echo("no --provider given; defaulting prompt-bearing tasks to claude headless")

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

    # A declared cohort split is validated BEFORE any worker call -
    # membership (unknown ids), overlap, duplicates, and bank-rev staleness.
    from fno.evals.bank import CohortError, bank_unchanged_since, cohorts_verdict, load_cohorts

    decl = None
    try:
        decl = load_cohorts(bank_dir)
    except CohortError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=2)
    if decl is not None:
        verdict = cohorts_verdict(decl, known_ids=[t.id for t in tasks])
        if not verdict.get("ok"):
            for err in verdict.get("errors") or ["unknown native cohort door failure"]:
                typer.echo(f"Error: cohort split refused: {err}", err=True)
            raise typer.Exit(code=2)
        if not bank_unchanged_since(decl, repo_root):
            typer.echo(
                f"Error: cohort split is pinned to bank rev {decl.bank_rev[:12]} but the "
                f"bank changed since; redeclare cohorts.yaml against the current bank.",
                err=True,
            )
            raise typer.Exit(code=2)
        typer.echo(
            f"validated cohort split: train={len(decl.train)} validation={len(decl.validation)} "
            f"qualification={len(decl.qualification)} (pinned {decl.bank_rev[:12]})"
        )

    swept = sweep_orphans(repo_root)
    if swept:
        typer.echo(f"swept {swept} orphaned eval worktree(s) from a prior run")

    all_passed = True
    for t in tasks:
        results = run_task(t, repeat=repeat, repo_root=repo_root, worker_provider=provider,
                           variant=variant, variant_ref=ref, lane=lane_coord,
                           experiment_id=cohort)
        passes = sum(1 for r in results if r.passed)
        passk = "PASS" if passes == repeat else "FAIL"
        typer.echo(f"  {t.tier:11} {t.id} [{variant}]: {passes}/{repeat} pass  (pass^{repeat}={passk})")
        for r in results:
            if not r.passed:
                typer.echo(f"      run {r.repeat_index}: {r.reason}")
        if passes < repeat:
            all_passed = False
    typer.echo("done." if all_passed else "done (some runs failed; see history).")
    raise typer.Exit(code=0)


@evals_app.command(
    "trend",
    context_settings={"ignore_unknown_options": True, "allow_extra_args": True},
)
def trend_command(ctx: typer.Context) -> None:
    """Score the recent window against the prior one; the fold is native
    (fno-agents evals-trend, d-b6cc1a2a). Exit 4 when regressed."""
    _forward_evals_native(ctx.args, "trend")


@evals_app.command(
    "report",
    context_settings={"ignore_unknown_options": True, "allow_extra_args": True},
)
def report_command(ctx: typer.Context) -> None:
    """Fold evals history: tiers, pass@1, pass^k, flakes, alarm. A pure argv
    forwarder (d-b6cc1a2a); the flags are the binary's. Exit 4 on alarm."""
    _forward_evals_native(ctx.args, "report")


def _forward_evals_native(extra: list[str], mode: str) -> None:
    """Resolve the history default and stale window, run the native fold,
    propagate its exit code (4 = alarm/regressed). Missing binary exits 2."""
    import subprocess

    from fno._subprocess_util import propagate_returncode
    from fno.config import load_settings
    from fno.paths import evals_history
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno doctor evals: the fno-agents binary was not found. It ships in the "
            "`pip install fno` wheel and with the plugin; reinstall fno or run "
            "`fno doctor update --rust`, or set FNO_AGENTS_BIN to its path.",
            err=True,
        )
        raise typer.Exit(code=2)
    try:
        stale_days = int(load_settings().evals.stale_days)
    except Exception:  # noqa: BLE001 - an unreadable config reads the default
        stale_days = 7
    argv = [
        str(binary), "evals-trend",
        "--mode", mode,
        "--history", str(evals_history()),
        "--stale-days", str(stale_days),
        *extra,
    ]
    result = subprocess.run(argv, check=False, capture_output=True, text=True)
    # Re-echo through typer so CliRunner-backed tests (and any caller that
    # wraps stdout) see the fold's output; the binary's trailing newline is
    # preserved with nl=False.
    if result.stdout:
        typer.echo(result.stdout, nl=False)
    if result.stderr:
        typer.echo(result.stderr, nl=False, err=True)
    raise typer.Exit(code=propagate_returncode(result.returncode))


@evals_app.command(
    "macro",
    context_settings={"ignore_unknown_options": True, "allow_extra_args": True},
)
def macro_command(ctx: typer.Context) -> None:
    """Find recurring labelled failure patterns in existing event journals."""
    import subprocess

    from fno._subprocess_util import propagate_returncode
    from fno.paths import event_journals
    from fno.rust_binary import resolve_binary

    # A pure argv forwarder: the flags are the binary's (operator ruling on
    # - the Python flag surface never grows), so this arm declares no
    # typer.Options of its own. It only resolves the journal defaults the
    # verb needs when the caller passed no --events.
    args = list(ctx.args)
    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno doctor evals macro: the fno-agents binary was not found. It ships in the "
            "`pip install fno` wheel and with the plugin; reinstall fno or run "
            "`fno doctor update --rust`, or set FNO_AGENTS_BIN to its path.",
            err=True,
        )
        raise typer.Exit(code=2)

    argv = [str(binary), "evals-macro", *args]
    if not any(a == "--events" or a.startswith("--events=") for a in args):
        for path in event_journals():
            argv += ["--events", str(path)]
    result = subprocess.run(argv, check=False, capture_output=True, text=True)
    # Re-echo through typer so CliRunner-backed tests (and any caller that
    # wraps stdout) see the fold's output; the binary's trailing newline is
    # preserved with nl=False.
    if result.stdout:
        typer.echo(result.stdout, nl=False)
    if result.stderr:
        typer.echo(result.stderr, nl=False, err=True)
    raise typer.Exit(code=propagate_returncode(result.returncode))


@evals_app.command("export")
def export_command(
    out: Path = typer.Option(..., "--out", help="Output JSONL path for the train-only rows."),
    bank: Optional[Path] = typer.Option(None, "--bank", help="Bank dir (default: <repo>/evals/bank)."),
    history: Optional[Path] = typer.Option(None, "--history", help="History JSONL (default: the evals history)."),
) -> None:
    """Export train-cohort history for prompt tuning.

    Refuses without a declared, natively valid, bank-current split; writes
    ONLY train rows, so held-out trajectories never enter a tuning view.
    Exit 0 exported / 1 no declared split / 2 split refused."""
    import json as _json

    from fno.evals.bank import (
        COHORTS_FILENAME,
        BankError,
        CohortError,
        bank_unchanged_since,
        cohorts_verdict,
        discover_bank,
        load_cohorts,
    )
    from fno.evals.report import load_rows
    from fno.paths import evals_history, resolve_canonical_repo_root

    bank_dir = _resolve_bank_dir(bank)
    try:
        decl = load_cohorts(bank_dir)
    except CohortError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=2)
    if decl is None:
        typer.echo(
            f"Error: no declared cohort split in {bank_dir} ({COHORTS_FILENAME}): a tuning "
            f"export must never be cut from an undeclared bank.",
            err=True,
        )
        raise typer.Exit(code=1)

    try:
        tasks = discover_bank(bank_dir)
    except BankError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=2)
    verdict = cohorts_verdict(decl, known_ids=[t.id for t in tasks])
    if not verdict.get("ok"):
        for err in verdict.get("errors") or ["unknown native cohort door failure"]:
            typer.echo(f"Error: cohort split refused: {err}", err=True)
        raise typer.Exit(code=2)
    try:
        repo_root = resolve_canonical_repo_root()
    except Exception:  # noqa: BLE001
        repo_root = Path.cwd()
    if not bank_unchanged_since(decl, repo_root):
        typer.echo(
            f"Error: cohort split is pinned to bank rev {decl.bank_rev[:12]} but the bank "
            f"changed since; redeclare cohorts.yaml against the current bank.",
            err=True,
        )
        raise typer.Exit(code=2)

    rows = load_rows(history or evals_history(), variant=None)
    train = set(decl.train)
    out.parent.mkdir(parents=True, exist_ok=True)
    count = 0
    with out.open("w", encoding="utf-8") as fh:
        for row in rows:
            if row.get("task_id") in train:
                fh.write(_json.dumps({"role": "train", **row}, separators=(",", ":"),
                                     ensure_ascii=False) + "\n")
                count += 1
    typer.echo(f"exported {count} train row(s) across {len(train)} declared task(s) -> {out}")


@evals_app.command("qualify")
def qualify_command(
    bank: Optional[Path] = typer.Option(None, "--bank", help="Bank dir (default: <repo>/evals/bank)."),
    history: Optional[Path] = typer.Option(None, "--history", help="History JSONL (default: the evals history)."),
) -> None:
    """Aggregate held-out qualification results.

    Counts and coverage only, never a held-out prompt or trace. Reports
    `{"qualified": false, "reason": ...}` (exit 0) when the split is absent,
    refused, stale, or has an empty qualification list. Exit 2 = setup error."""
    import json as _json

    from fno.evals import history as _history
    from fno.evals.bank import (
        COHORTS_FILENAME,
        BankError,
        CohortError,
        bank_unchanged_since,
        cohorts_verdict,
        discover_bank,
        load_cohorts,
    )
    from fno.paths import evals_history, resolve_canonical_repo_root

    bank_dir = _resolve_bank_dir(bank)
    try:
        tasks = discover_bank(bank_dir)
    except BankError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=2)

    def _unqualified(reason: str) -> None:
        typer.echo(_json.dumps({"qualified": False, "reason": reason}, ensure_ascii=False))
        raise typer.Exit(code=0)

    try:
        decl = load_cohorts(bank_dir)
    except CohortError as exc:
        raise typer.Exit(code=2) from exc
    if decl is None:
        _unqualified(f"no declared cohort split ({COHORTS_FILENAME})")
    assert decl is not None  # _unqualified raised otherwise
    if not decl.qualification:
        _unqualified("no declared qualification cohort")
    verdict = cohorts_verdict(decl, known_ids=[t.id for t in tasks],
                              task_ids=[t.id for t in tasks])
    if not verdict.get("ok"):
        _unqualified("cohort split refused: " + "; ".join(
            verdict.get("errors") or ["unknown native cohort door failure"]))
    try:
        repo_root = resolve_canonical_repo_root()
    except Exception:  # noqa: BLE001
        repo_root = Path.cwd()
    if not bank_unchanged_since(decl, repo_root):
        _unqualified(f"bank changed since the pinned rev {decl.bank_rev[:12]}")

    roles: dict = verdict.get("roles") or {}
    qual_ids = {tid for tid, role in roles.items() if role == "qualification"}

    rows = [(ln, row) for ln, row in
            _history.iter_rows_tolerant(history or evals_history())
            if row.get("task_id") in qual_ids]

    # The batch native verdict decides which rows are eligible evidence;
    # classify_rows numbers lines exactly like iter_rows_tolerant does.
    import subprocess as _subprocess

    from fno.evals.bank import _door_binary

    binary = _door_binary()
    verdicts: dict[int, dict] = {}
    if binary is not None and rows:
        try:
            proc = _subprocess.run(
                [str(binary), "evals-attempt", "--rows", str(history or evals_history()),
                 "--expected-rev", decl.bank_rev],
                capture_output=True, text=True, timeout=30,
            )
            if proc.returncode == 0:
                parsed = _json.loads(proc.stdout.strip().splitlines()[-1])
                verdicts = {v["line"]: v for v in parsed}
        except Exception:  # noqa: BLE001 - an unreadable batch verdict degrades to legacy
            verdicts = {}

    by_status: dict[str, int] = {}
    valid = 0
    passes = 0
    covered_tasks: set = set()
    wrong_rev = 0
    legacy = 0
    for ln, row in rows:
        v = verdicts.get(ln)
        if not isinstance(v, dict):
            v = {}
        status = str(v.get("status") or "legacy")
        rev_match = v.get("rev_match")
        if rev_match is False:
            wrong_rev += 1
            continue
        if status == "legacy":
            legacy += 1
            continue
        by_status[status] = by_status.get(status, 0) + 1
        if status == "graded":
            valid += 1
            passes += bool(v.get("graded"))
            covered_tasks.add(row.get("task_id"))
    declared = len(qual_ids)
    typer.echo(_json.dumps({
        "qualified": True,
        "bank_rev": decl.bank_rev,
        "declared_tasks": declared,
        "tasks_with_valid_grades": len(covered_tasks),
        "missing_tasks": declared - len(covered_tasks),
        "valid_grades": valid,
        "passes": passes,
        "attempts": by_status,
        "excluded": {"wrong_rev": wrong_rev, "legacy": legacy},
    }, ensure_ascii=False))
    raise typer.Exit(code=0)


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
