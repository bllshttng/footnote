"""Repository lint commands exposed through ``fno lint``."""
from __future__ import annotations

import ast
import re
import subprocess
from pathlib import Path
from typing import Optional, cast

import click
import typer


app = typer.Typer(help="Repository lint checks", no_args_is_help=True)


# x-71b6 In-N-Out menu ratchet: the advertised command surface stays small.
# These are the two knobs a maintainer touches to widen the menu, on purpose,
# in a one-line diff that shows up in review - the display-surface counterpart
# of the control-plane LOC ratchet. New verbs default to hidden; promotion is a
# deliberate act that must fit under these caps.
MENU_CAP_TOP_LEVEL = 10
MENU_CAP_SUB_APP = 12


# Single-line argv shapes that are intentionally still owned by a provider or
# a legacy one-shot seam. New session spawns belong behind dispatch_spawn; a
# contributor adding a new shape must either migrate it or add the exact file
# here in the same change. This is deliberately a narrow grep-style guard, not
# an AST claim: multi-line assembly is documented coverage debt.
SPAWN_SHAPE_ALLOWLIST = frozenset(
    {
        "cli/src/fno/agents/dispatch.py",
        "cli/src/fno/agents/providers/claude.py",
        "cli/src/fno/agents/providers/codex.py",
        "cli/src/fno/graph/maintain.py",
        "cli/src/fno/graph/triage.py",
        "cli/src/fno/inbox/triage.py",
        "cli/src/fno/pr_watch/_dispatch.py",
        "cli/src/fno/review/scorers/claude_scorer.py",
        "cli/src/fno/skill_diff/synthesize.py",
        # Shell-form hit surfaced by the .sh scan; live memory-pass path,
        # census-tracked migration work (spawn census, open row).
        "scripts/memory/post-merge-pass.sh",
    }
)
_SPAWN_SHAPE_RE = re.compile(
    r"\[\s*['\"](?:claude|codex)['\"]\s*,\s*['\"](?:--print|--bg|-p|--exec)['\"]"
)
# Shell-form single-line launches (`claude --bg "$prompt"`); .sh files only,
# where the argv-list form above can never appear.
_SHELL_SPAWN_RE = re.compile(r"\bclaude\s+(?:--print|--bg|-p)\b|\bcodex\s+(?:--exec|exec)\b")
_SOURCE_SUFFIXES = frozenset({".py", ".sh"})


def _spawn_shape_files(repo_root: Path) -> list[Path]:
    """Return production source files covered by the narrow spawn-shape scan."""
    roots = [repo_root / "cli" / "src" / "fno", repo_root / "scripts"]
    files: list[Path] = []
    for root in roots:
        if root.is_dir():
            files.extend(
                p
                for p in root.rglob("*")
                if p.is_file()
                and p.suffix in _SOURCE_SUFFIXES
                and "tests" not in p.parts
            )
    return sorted(files)


def _spawn_shape_violations(repo_root: Path) -> list[str]:
    violations: list[str] = []
    for path in _spawn_shape_files(repo_root):
        rel = path.relative_to(repo_root).as_posix()
        if rel in SPAWN_SHAPE_ALLOWLIST:
            continue
        for line_no, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            match = _SPAWN_SHAPE_RE.search(line)
            if match is None and path.suffix == ".sh":
                match = _SHELL_SPAWN_RE.search(line)
            if match is not None:
                violations.append(
                    f"{rel}:{line_no}: hand-assembled session spawn shape "
                    f"{match.group(0)!r}"
                )
    return violations


@app.command("spawn-paths")
def spawn_paths() -> None:
    """Reject new single-line hand-assembled Claude/Codex session argv shapes.

    The allowlist is intentionally explicit and lives next to this lint. The
    scan does not claim to catch multi-line argv assembly; those sites remain
    census-backed migration work until they move behind ``dispatch_spawn``.
    """
    violations = _spawn_shape_violations(_repo_root())
    if violations:
        typer.echo("spawn-paths: violations:", err=True)
        for violation in violations:
            typer.echo(f"  {violation}", err=True)
        typer.echo(
            "\nFix: route session launches through fno agents spawn / "
            "dispatch_spawn, or add the exact source file to "
            "SPAWN_SHAPE_ALLOWLIST in cli/src/fno/lint_cli.py with a census-backed reason.",
            err=True,
        )
        raise typer.Exit(1)
    typer.echo("spawn-paths: ok")


def _visible_command_names(group: click.Group) -> list[str]:
    """Non-hidden subcommand names of a Click group, no module imports.

    Lazy top-level entries resolve to hidden-aware stubs, so this reads the
    curated surface straight from the registry the same way `fno --help` does.
    """
    ctx = click.Context(group)
    names: list[str] = []
    for name in group.list_commands(ctx):
        cmd = group.get_command(ctx, name)
        if cmd is not None and not cmd.hidden:
            names.append(name)
    return names


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


@app.command("menu-caps")
def menu_caps() -> None:
    """Enforce the In-N-Out menu caps (x-71b6): <=10 advertised top-level verbs,
    <=12 advertised verbs per sub-app. New verbs default to hidden; promoting one
    past a cap fails here until it is hidden again or the cap constant is raised
    in a deliberate one-line diff. Introspects the registry - no repo scripts, so
    it runs from a bare install too.
    """
    import importlib

    import typer.main

    from fno.cli import LAZY_SUBCOMMANDS, app as root_app

    root = typer.main.get_command(root_app)
    top_visible = _visible_command_names(cast("click.Group", root))
    failures: list[str] = []

    if len(top_visible) > MENU_CAP_TOP_LEVEL:
        over = ", ".join(top_visible[MENU_CAP_TOP_LEVEL:])
        failures.append(
            f"top-level menu advertises {len(top_visible)} commands "
            f"(cap {MENU_CAP_TOP_LEVEL}); over the cap: {over}.\n"
            f"  Remedy 1: mark it hidden - add {{\"hidden\": True}} to its "
            f"LAZY_SUBCOMMANDS entry (or hidden=True on its @app.command).\n"
            f"  Remedy 2: raise MENU_CAP_TOP_LEVEL (a deliberate one-line diff)."
        )

    # Every group sub-app is capped, INCLUDING hidden top-level ones: opening
    # `fno mail --help` renders mail's own menu even though `mail` is hidden from
    # the top-level surface, so that menu must stay curated too. Iterate the whole
    # registry, not just the advertised entries. Dedupe by import target so an
    # alias (e.g. `graph` -> `backlog`) is checked once.
    seen_targets: set[str] = set()
    for name, entry in LAZY_SUBCOMMANDS.items():
        import_path = entry[0]
        if import_path in seen_targets:
            continue
        seen_targets.add(import_path)
        module_path, _, attr = import_path.rpartition(":")
        try:
            obj = getattr(importlib.import_module(module_path), attr, None)
        except Exception as exc:  # noqa: BLE001 - a lint must degrade, not crash
            typer.echo(f"menu-caps: skipped sub-app {name!r} (import failed: {exc})", err=True)
            continue
        if not isinstance(obj, typer.Typer):
            continue  # single-command entry, not a group
        sub_group = typer.main.get_command(obj)
        # Duck-type, not isinstance(click.Group): Typer bundles a vendored click
        # (typer._click), so a TyperGroup is NOT an instance of the top-level
        # `click.Group` - an isinstance check here silently skips every sub-app.
        if not hasattr(sub_group, "list_commands"):
            continue
        sub_visible = _visible_command_names(cast("click.Group", sub_group))
        if len(sub_visible) > MENU_CAP_SUB_APP:
            over = ", ".join(sub_visible[MENU_CAP_SUB_APP:])
            failures.append(
                f"sub-app `fno {name}` advertises {len(sub_visible)} verbs "
                f"(cap {MENU_CAP_SUB_APP}); over the cap: {over}.\n"
                f"  Remedy 1: mark it hidden - hidden=True on the @command / add_typer.\n"
                f"  Remedy 2: raise MENU_CAP_SUB_APP (a deliberate one-line diff)."
            )

    if failures:
        for f in failures:
            typer.echo(f"menu-caps: FAIL\n{f}", err=True)
        raise typer.Exit(1)
    typer.echo(f"menu-caps: ok (top-level {len(top_visible)}/{MENU_CAP_TOP_LEVEL})")


@app.command("stale-skill-refs")
def stale_skill_refs() -> None:
    """Audit for stale references to cut, demoted, or merged skills.

    Re-homed from the retired `fno consolidation audit` (x-71b6): a lint gate
    wearing a command costume belongs under `fno lint`. Thin wrapper over the
    source-of-truth bash gate scripts/ci/check-no-stale-skill-refs.sh; exit code
    matches it (0 clean, 1 stale references, 2 script error).
    """
    from fno._subprocess_util import propagate_returncode
    from fno.paths import resolve_repo_root

    repo_root = Path(resolve_repo_root())
    script = repo_root / "scripts" / "ci" / "check-no-stale-skill-refs.sh"
    if not script.exists():
        typer.echo(f"audit script not found at {script}", err=True)
        raise typer.Exit(code=2)
    try:
        result = subprocess.run(["bash", str(script)], cwd=repo_root)
    except FileNotFoundError as exc:
        typer.echo(f"failed to run audit script: {exc}", err=True)
        raise typer.Exit(code=2)
    raise typer.Exit(code=propagate_returncode(result.returncode))
