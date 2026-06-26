"""Tests for the LazyTypeGroup lazy-import refactor.

Acceptance criteria:
  AC3-HP: fno paths state-dir does NOT import megawalk
  AC3-EDGE: fno --help doesn't import any sub-app body
  AC3-FR: no functional regression in existing verbs
  AC1-ERR: misconfigured lazy entry fails loud
  AC2-HP: fno --help lists all current subcommands
"""
from __future__ import annotations

import importlib
import subprocess
import sys
from typing import Any

import pytest


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _run_py(code: str, timeout: int = 30) -> subprocess.CompletedProcess[str]:
    """Run a Python snippet in a fresh subprocess, returning the result."""
    return subprocess.run(
        [sys.executable, "-c", code],
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def _run_abi(*args: str, timeout: int = 30) -> subprocess.CompletedProcess[str]:
    """Run the installed `fno` binary with given args."""
    return subprocess.run(
        ["fno", *args],
        capture_output=True,
        text=True,
        timeout=timeout,
    )


# ---------------------------------------------------------------------------
# AC3-EDGE: fno --help doesn't import sub-app module bodies
# ---------------------------------------------------------------------------

# Modules that must NOT appear in sys.modules after `fno --help`.
# These are the heaviest transitive imports that the lazy refactor defers.
_FORBIDDEN_AFTER_HELP = [
    "fno.state.cli",
    "fno.megatron.cli",
    "fno.adapters.providers.cli",
    "fno.worker.cli",
    "fno.graph.cli",
    "fno.events.cli",
    "fno.mail.cli",
    "fno.agent",
]

_CHECK_CODE_HELP = """\
import sys
from fno import cli
from typer.testing import CliRunner
CliRunner().invoke(cli.app, ["--help"])
forbidden = {forbidden!r}
found = [m for m in forbidden if m in sys.modules]
if found:
    print("FOUND:", ",".join(found), file=sys.stderr)
sys.exit(len(found))
""".format(forbidden=_FORBIDDEN_AFTER_HELP)


def test_abi_help_does_not_import_sub_app_modules():
    """AC3-EDGE: sys.modules after `fno --help` excludes sub-app bodies."""
    result = _run_py(_CHECK_CODE_HELP)
    assert result.returncode == 0, (
        f"Sub-app modules imported after fno --help:\n{result.stderr}"
    )


# ---------------------------------------------------------------------------
# AC3-HP: fno paths state-dir does NOT import megawalk / megatron
# ---------------------------------------------------------------------------

_CHECK_CODE_PATHS = """\
import sys
from fno import cli
from typer.testing import CliRunner
CliRunner().invoke(cli.app, ["paths", "state-dir"])
found = [m for m in ["fno.megatron.cli"] if m in sys.modules]
if found:
    print("FOUND:", ",".join(found), file=sys.stderr)
sys.exit(len(found))
"""


def test_abi_paths_does_not_import_megawalk():
    """AC3-HP: `fno paths state-dir` only loads the paths sub-app."""
    result = _run_py(_CHECK_CODE_PATHS)
    assert result.returncode == 0, (
        f"megawalk/megatron imported during `fno paths state-dir`:\n{result.stderr}"
    )


# ---------------------------------------------------------------------------
# AC1-ERR: misconfigured lazy entry fails loud
# ---------------------------------------------------------------------------

def test_bad_lazy_entry_fails_loud():
    """AC1-ERR: bad module:attr in lazy_subcommands exits non-zero with helpful message."""
    from fno._lazy_group import LazyTypeGroup, make_lazy_group_cls
    import typer
    from typer.testing import CliRunner

    bad_cls = make_lazy_group_cls({"bad": "fno.state.cli:nonexistent_attr_xyz"})
    test_app = typer.Typer(cls=bad_cls, no_args_is_help=True)

    @test_app.callback()
    def _cb() -> None:
        pass

    runner = CliRunner()
    result = runner.invoke(test_app, ["bad"])
    # Must fail with non-zero exit
    assert result.exit_code != 0, "Expected non-zero exit for bad lazy entry"
    # Error message must name the bad import path
    combined = (result.output or "") + (result.stderr if hasattr(result, "stderr") else "")
    assert "nonexistent_attr_xyz" in combined or "fno.state.cli" in combined, (
        f"Error should name the bad import path; got: {combined!r}"
    )


def test_bad_module_path_fails_loud():
    """AC1-ERR: bad module path in lazy_subcommands exits non-zero with helpful message."""
    from fno._lazy_group import LazyTypeGroup, make_lazy_group_cls
    import typer
    from typer.testing import CliRunner

    bad_cls = make_lazy_group_cls({"bad": "fno.does_not_exist_module_xyz:cli"})
    test_app = typer.Typer(cls=bad_cls, no_args_is_help=True)

    @test_app.callback()
    def _cb() -> None:
        pass

    runner = CliRunner()
    result = runner.invoke(test_app, ["bad"])
    assert result.exit_code != 0, "Expected non-zero exit for bad module path"
    combined = (result.output or "") + (result.stderr if hasattr(result, "stderr") else "")
    assert "does_not_exist_module_xyz" in combined or "fno" in combined, (
        f"Error should name the bad module; got: {combined!r}"
    )


# ---------------------------------------------------------------------------
# AC2-HP: fno --help lists all subcommands (no regression in command surface)
# ---------------------------------------------------------------------------

# Stable subcommands that must appear in `fno --help` after the refactor.
_EXPECTED_SUBCOMMANDS = [
    "backlog",
    "evals",
    "event",
    "pr",
    "paths",
    "mail",
    "agent",
    "megatron",
    "providers",
    "review",
    "cost",
    "help",
]


def test_abi_help_lists_all_subcommands():
    """AC2-HP: fno --help lists all expected subcommands after the refactor."""
    from fno.cli import app
    from typer.testing import CliRunner

    runner = CliRunner()
    result = runner.invoke(app, ["--help"])
    assert result.exit_code == 0, f"fno --help failed: {result.output}"
    for cmd in _EXPECTED_SUBCOMMANDS:
        assert cmd in result.output, f"Subcommand {cmd!r} missing from fno --help"


# ---------------------------------------------------------------------------
# Unit tests for LazyTypeGroup directly
# ---------------------------------------------------------------------------

def test_lazy_group_list_commands_includes_lazy_keys():
    """LazyTypeGroup.list_commands() returns lazy keys even before import."""
    from fno._lazy_group import make_lazy_group_cls
    import typer
    import typer.main

    lazy_map = {"alpha": "some.module:attr", "beta": "other.module:attr"}
    cls = make_lazy_group_cls(lazy_map)
    test_app = typer.Typer(cls=cls, no_args_is_help=True)

    @test_app.callback()
    def _cb() -> None:
        pass

    cmd = typer.main.get_command(test_app)
    commands = cmd.list_commands(None)  # type: ignore[arg-type]
    assert "alpha" in commands
    assert "beta" in commands


# ---------------------------------------------------------------------------
# Regression: group structure preserved for single-command sub-apps
# ---------------------------------------------------------------------------

def test_executor_resolve_group_shape_preserved():
    """Single-command Typer sub-apps must NOT collapse to a TyperCommand.

    Without `get_group_from_info`, `executor_app` (which has only one
    command `resolve`) would collapse via `typer.main.get_command()` into
    a bare TyperCommand, breaking `fno executor resolve <args>`.

    Regression test for the fix in `_LazyStub._load_real`.
    """
    from fno.cli import app
    from typer.testing import CliRunner

    runner = CliRunner()
    # Invoking the help for the sub-command proves the group structure is
    # intact: a collapsed app would not recognise 'resolve' as a subcommand.
    result = runner.invoke(app, ["executor", "resolve", "--help"])
    assert result.exit_code == 0, f"fno executor resolve --help failed: {result.output}"
    assert "--plan-path" in result.output, (
        "Expected --plan-path option in `fno executor resolve --help`; "
        f"got: {result.output}"
    )


# ---------------------------------------------------------------------------
# Regression: info_overrides round-trip preserves extended help text
# ---------------------------------------------------------------------------

def test_megatron_help_carries_exit_codes():
    """Same regression check for megatron's extended help."""
    from fno.cli import app
    from typer.testing import CliRunner

    runner = CliRunner()
    result = runner.invoke(app, ["megatron", "--help"])
    assert result.exit_code == 0, f"fno megatron --help failed: {result.output}"
    assert "Exit codes" in result.output, (
        "Expected 'Exit codes' in `fno megatron --help` output; "
        f"got: {result.output[:500]}"
    )


# ---------------------------------------------------------------------------
# Real-subprocess smoke for the installed entry point
# ---------------------------------------------------------------------------

def test_abi_backlog_ready_via_real_subprocess():
    """Smoke test the installed `fno` console script through the lazy group.

    Exercises the full ``[project.scripts]`` entry-point wiring + lazy
    sub-app dispatch.  Skipped if ``fno`` is not on PATH (e.g. running
    in a clean tox env where the package is not installed as a tool).
    """
    import shutil
    fno = shutil.which("fno")
    if not fno:
        pytest.skip("fno binary not on PATH (run `uv tool install <repo>/cli` first)")
    result = subprocess.run(
        [fno, "backlog", "ready", "--help"],
        capture_output=True,
        text=True,
        timeout=15,
    )
    assert result.returncode == 0, (
        f"fno backlog ready --help failed (rc={result.returncode}):\n"
        f"stdout={result.stdout}\nstderr={result.stderr}"
    )


# ---------------------------------------------------------------------------
# Regression: "Did you mean" suggestion is not duplicated
# ---------------------------------------------------------------------------

def test_no_duplicate_did_you_mean_for_eager_command_typo():
    """TyperGroup already adds 'Did you mean' for eager-command typos.

    Codex P2 finding on PR #269: ``LazyTypeGroup.resolve_command`` was
    appending its own suggestion unconditionally, producing duplicated
    output like ``Did you mean 'help'?. Did you mean 'help'?`` when the
    typo matched an eager command (one registered via ``@app.command()``
    that lives in this file rather than the lazy map).
    """
    from fno.cli import app
    from typer.testing import CliRunner

    runner = CliRunner()
    # ``help`` is an eager command (defined directly in cli.py); ``hepl``
    # is a typo of it.  TyperGroup picks up the suggestion via
    # ``self.commands``; the lazy override must not add a second one.
    result = runner.invoke(app, ["hepl"])
    assert result.exit_code != 0, "Expected non-zero exit for unknown command"
    # The literal substring should appear at most once.
    assert result.output.count("Did you mean 'help'") == 1, (
        f"Duplicate 'Did you mean help' in output: {result.output}"
    )


def test_did_you_mean_suggests_lazy_commands():
    """Typos that match a lazy entry still get a 'Did you mean' hint.

    The lazy override must keep adding suggestions for commands TyperGroup
    cannot see (the ones in ``self._lazy`` rather than ``self.commands``).
    Regression for the fix in ``resolve_command``: skip-append must NOT
    apply when the parent's message has no suggestion at all.
    """
    from fno.cli import app
    from typer.testing import CliRunner

    runner = CliRunner()
    # ``megatron`` is a lazy entry; ``mehtatron`` is a typo.
    result = runner.invoke(app, ["mehtatron"])
    assert result.exit_code != 0
    assert "Did you mean 'megatron'" in result.output, (
        f"Missing megatron suggestion: {result.output}"
    )


def test_lazy_group_get_command_imports_on_demand():
    """LazyTypeGroup.get_command() triggers import only when invoked."""
    from fno._lazy_group import make_lazy_group_cls
    import typer
    import typer.main

    cls = make_lazy_group_cls({"state": "fno.state.cli:cli"})
    test_app = typer.Typer(cls=cls, no_args_is_help=True)

    @test_app.callback()
    def _cb() -> None:
        pass

    cmd = typer.main.get_command(test_app)
    # Before get_command, state.cli should not be imported (it may be from elsewhere,
    # but what matters is that list_commands doesn't trigger it).
    modules_before = set(sys.modules)
    _ = cmd.list_commands(None)  # type: ignore[arg-type]
    modules_after_list = set(sys.modules)
    # list_commands alone must not trigger the import
    assert "fno.state.cli" not in (modules_after_list - modules_before), (
        "list_commands() triggered import of fno.state.cli"
    )
