"""Tests for the LazyTypeGroup lazy-import refactor.

Acceptance criteria:
  AC3-HP: fno paths state-dir does NOT import heavy sub-apps
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
# AC3-HP: fno paths state-dir does NOT import heavy sub-apps
# ---------------------------------------------------------------------------

_CHECK_CODE_PATHS = """\
import sys
from fno import cli
from typer.testing import CliRunner
CliRunner().invoke(cli.app, ["paths", "state-dir"])
found = [m for m in ["fno.adapters.providers.cli"] if m in sys.modules]
if found:
    print("FOUND:", ",".join(found), file=sys.stderr)
sys.exit(len(found))
"""


def test_abi_paths_does_not_import_heavy_subapps():
    """AC3-HP: `fno paths state-dir` only loads the paths sub-app."""
    result = _run_py(_CHECK_CODE_PATHS)
    assert result.returncode == 0, (
        f"heavy sub-app imported during `fno paths state-dir`:\n{result.stderr}"
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
# The curated top-level menu (x-71b6 In-N-Out tiering). `fno --help` advertises
# only these; everything else is hidden but invocable, listed by `fno help --all`.
_ADVERTISED_SUBCOMMANDS = [
    "help",
    "backlog",
    "agents",
    "config",
    "setup",
    "whoami",
    "doctor",
    "test",
    "update",
]

# A sample of the hidden tier - present under `help --all`, absent from `--help`.
# Distinctive names only: short verbs (pr/cost/state) are substrings of ordinary
# help prose, so a raw substring leak-check on them is unreliable.
_HIDDEN_SUBCOMMANDS = [
    "evals", "providers", "carveout", "consolidation", "scoreboard", "stub-manifest",
]


def _strip_ansi(text: str) -> str:
    import re
    return re.sub(r"\x1b\[[0-9;]*m", "", text)


def test_abi_help_lists_only_advertised_menu():
    """AC1-HP: fno --help advertises the curated menu and hides the rest."""
    from fno.cli import app
    from typer.testing import CliRunner

    runner = CliRunner()
    result = runner.invoke(app, ["--help"])
    assert result.exit_code == 0, f"fno --help failed: {result.output}"
    plain = _strip_ansi(result.output)
    for cmd in _ADVERTISED_SUBCOMMANDS:
        assert cmd in plain, f"advertised {cmd!r} missing from fno --help"
    for cmd in _HIDDEN_SUBCOMMANDS:
        assert cmd not in plain, f"hidden {cmd!r} leaked into fno --help"


def test_help_all_lists_every_command_including_hidden():
    """AC3-UI: `fno help --all` is the full-surface door - advertised + hidden."""
    from fno.cli import app
    from typer.testing import CliRunner

    runner = CliRunner()
    result = runner.invoke(app, ["help", "--all"])
    assert result.exit_code == 0, f"fno help --all failed: {result.output}"
    # Names itself as the full surface.
    assert "full command surface" in result.output
    for cmd in _ADVERTISED_SUBCOMMANDS + _HIDDEN_SUBCOMMANDS:
        assert cmd in result.output, f"{cmd!r} missing from fno help --all"


def test_help_all_never_imports_command_modules(monkeypatch):
    """AC3-UI: `help --all` renders from registry strings, so a broken command
    module still yields a full listing and a 0 exit (no module import)."""
    import builtins
    from fno.cli import LAZY_SUBCOMMANDS, app
    from typer.testing import CliRunner

    # The set of command-implementation modules the registry points at. If
    # help --all imported any of them to build its listing, this would break it.
    command_modules = {
        entry[0].split(":", 1)[0] for entry in LAZY_SUBCOMMANDS.values()
    }
    real_import = builtins.__import__

    def _boom(name, *args, **kwargs):
        if name in command_modules:
            raise ImportError(f"simulated broken command module: {name}")
        return real_import(name, *args, **kwargs)

    monkeypatch.setattr(builtins, "__import__", _boom)
    runner = CliRunner()
    result = runner.invoke(app, ["help", "--all"])
    assert result.exit_code == 0, f"help --all should survive broken modules: {result.output}"
    plain = _strip_ansi(result.output)
    assert "evals" in plain and "backlog" in plain


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
# Real-subprocess smoke for the installed entry point
# ---------------------------------------------------------------------------

def test_abi_backlog_ready_via_real_subprocess():
    """Smoke test the installed `fno-py` console script through the lazy group.

    Exercises the full ``[project.scripts]`` entry-point wiring + lazy
    sub-app dispatch.  Skipped if ``fno-py`` is not on PATH (e.g. running
    in a clean tox env where the package is not installed as a tool).
    """
    import shutil
    fno = shutil.which("fno-py")
    if not fno:
        pytest.skip("fno-py console script not on PATH (run `uv tool install <repo>/cli` first)")
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
    # ``backlog`` is a lazy entry; ``backlg`` is a typo.
    result = runner.invoke(app, ["backlg"])
    assert result.exit_code != 0
    assert "Did you mean 'backlog'" in result.output, (
        f"Missing backlog suggestion: {result.output}"
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


# ---------------------------------------------------------------------------
# config <-> graph import cycle: broken by the fno.config_io leaf (x-7fdd).
# Guards the invariant that both packages import at module scope in EITHER
# order without ImportError, and that the leaf holds no back-edge.
# ---------------------------------------------------------------------------

@pytest.mark.parametrize("order", ["fno.config, fno.graph", "fno.graph, fno.config"])
def test_config_graph_import_cycle_broken(order: str):
    """A fresh interpreter can import config and graph in either order."""
    result = _run_py(f"import {order}; print('ok')")
    assert result.returncode == 0, (
        f"import order '{order}' failed:\n{result.stderr}"
    )
    assert "ok" in result.stdout


def test_config_io_is_a_leaf():
    """The extracted leaf must never import fno.config or fno.graph (a back-edge
    reintroduces the cycle). Assert on real import statements, not the docstring."""
    import re

    import fno.config_io as leaf

    src = open(leaf.__file__).read()
    assert not re.search(r"^\s*(from|import)\s+fno\.(config|graph)\b", src, re.M), (
        "fno.config_io must not import fno.config or fno.graph"
    )
    # re-export shim: config exposes the moved names as the SAME objects
    import fno.config as cfg

    assert cfg.read_config_flat is leaf.read_config_flat
    assert cfg._deep_merge is leaf._deep_merge


def test_config_first_import_does_not_freeze_graph_path_to_fallback(tmp_path):
    """config's graph._constants import stays function-local: a top-level one makes
    `import fno.config` eagerly load the graph package during config's partial init,
    which freezes store.read_graph's GRAPH_JSON default to the ~/.fno fallback and
    silently ignores a configured paths.graph_json (Codex P1). Regression guard:
    with a graph_json override, config-first import must still resolve it."""
    import os

    cfg = tmp_path / "config.toml"
    graph_json = tmp_path / "state" / "mygraph.json"
    cfg.write_text(f'[paths]\ngraph_json = "{graph_json}"\n')

    code = (
        "import fno.config, fno.graph, inspect\n"  # config-first (the risky order)
        "import fno.graph.store as store\n"
        "d = inspect.signature(store.read_graph).parameters['path'].default\n"
        "print(str(d))\n"
    )
    result = subprocess.run(
        [sys.executable, "-c", code],
        capture_output=True, text=True, timeout=60,
        env={**os.environ, "FNO_CONFIG": str(cfg)},
    )
    assert result.returncode == 0, result.stderr
    assert "mygraph.json" in result.stdout, (
        f"read_graph default froze to the fallback, not the configured path:\n{result.stdout}"
    )
