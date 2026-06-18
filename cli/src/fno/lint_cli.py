"""Repository lint commands exposed through ``fno lint``."""
from __future__ import annotations

import ast
import subprocess
from pathlib import Path
from typing import Optional

import typer


app = typer.Typer(help="Repository lint checks", no_args_is_help=True)


def _repo_root() -> Path:
    result = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        typer.echo(
            "fno lint: git rev-parse failed; run from inside the repo",
            err=True,
        )
        raise typer.Exit(2)
    return Path(result.stdout.strip())


@app.command("flock-pattern")
def flock_pattern(
    dispatch_path: Optional[Path] = typer.Option(
        None,
        "--dispatch-path",
        help="Override dispatch.py path for tests or targeted linting.",
    ),
) -> None:
    """Forbid open-coded agent flock + registry re-read patterns."""
    from fno.paths import resolve_repo_root

    script = resolve_repo_root() / "scripts" / "lint-flock-pattern.sh"
    if not script.is_file():
        typer.echo(
            "fno lint flock-pattern: this verb lints the repo's own source and "
            "needs the footnote checkout's lint scripts, which a bare "
            "`pip install fno` does not ship. Run it from a clone (or install "
            "the plugin).",
            err=True,
        )
        raise typer.Exit(2)
    argv = ["bash", str(script)]
    if dispatch_path is not None:
        argv.append(str(dispatch_path))
    result = subprocess.run(argv)
    raise typer.Exit(result.returncode)


def _is_subprocess_stdout(value: ast.AST) -> bool:
    return (
        isinstance(value, ast.Attribute)
        and value.attr == "STDOUT"
        and isinstance(value.value, ast.Name)
        and value.value.id == "subprocess"
    )


def _stdout_merge_lines(tree: ast.AST) -> list[int]:
    lines: list[int] = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        for keyword in node.keywords:
            if keyword.arg == "stderr" and _is_subprocess_stdout(keyword.value):
                lines.append(keyword.value.lineno)
    return lines


def _has_stdout_merge_justification(source_lines: list[str], line_no: int) -> bool:
    window_start = max(0, line_no - 12)
    prior_window = source_lines[window_start:line_no - 1]
    current_comment = source_lines[line_no - 1].partition("#")[2]
    window = "\n".join([*prior_window, current_comment]).lower()
    return (
        "locked decision" in window
        or "stderr=stdout" in window
        or "stderr=subprocess.stdout" in window
        or "stdout-merge" in window
    )


@app.command("provider-stderr-merge")
def provider_stderr_merge(
    providers_dir: Optional[Path] = typer.Option(
        None,
        "--providers-dir",
        help="Override provider directory for tests or targeted linting.",
    ),
) -> None:
    """Require justification for provider stderr/stdout pipe merging."""
    root = (
        providers_dir
        if providers_dir is not None
        else _repo_root() / "cli" / "src" / "fno" / "agents" / "providers"
    )
    if not root.is_dir():
        typer.echo(f"provider-stderr-merge: providers dir not found: {root}", err=True)
        raise typer.Exit(2)

    violations: list[str] = []
    for path in sorted(root.glob("*.py")):
        source = path.read_text(encoding="utf-8")
        tree = ast.parse(source, filename=str(path))
        source_lines = source.splitlines()
        for line_no in _stdout_merge_lines(tree):
            if not _has_stdout_merge_justification(source_lines, line_no):
                violations.append(
                    f"{path.name}:{line_no}: stderr=subprocess.STDOUT "
                    "requires nearby provider-specific justification"
                )

    if violations:
        typer.echo("provider-stderr-merge: violations:", err=True)
        for violation in violations:
            typer.echo(f"  {violation}", err=True)
        typer.echo(
            "\nFix: add a nearby Locked Decision/comment explaining why this "
            "provider may safely merge stderr into stdout, or drain stderr "
            "separately.",
            err=True,
        )
        raise typer.Exit(1)

    typer.echo("provider-stderr-merge: ok")


@app.command("shellout-drift")
def shellout_drift(
    no_degrade: bool = typer.Option(
        False,
        "--no-degrade",
        help="Skip the degrade proof (static scan only). Tests/diagnostics; CI runs the full check.",
    ),
) -> None:
    """Forbid repo-root shell-outs without a proven clone-only degrade path (US4).

    Scans cli/src/fno/ for verbs that bash-exec a resolve_repo_root()/
    resolve_plugin_script()-rooted script; every such script must be on the
    CLONE_ONLY_SCRIPTS allowlist (scripts/lint/.clone-only-scripts.txt) and each
    allowlisted verb must degrade gracefully on a bare install. Fail-closed.
    """
    from fno import lint_shellout_drift

    report = lint_shellout_drift.run(do_degrade=not no_degrade)
    stream_err = report.exit_code != 0
    for line in report.lines:
        typer.echo(line, err=stream_err)
    raise typer.Exit(report.exit_code)
