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


def _run_fno(*args: str, timeout: int = 30) -> subprocess.CompletedProcess[str]:
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


def test_fno_help_does_not_import_sub_app_modules():
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


def test_fno_paths_does_not_import_heavy_subapps():
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
    "evals", "carveout", "scoreboard", "stub-manifest",
]


def _strip_ansi(text: str) -> str:
    import re
    return re.sub(r"\x1b\[[0-9;]*m", "", text)


def test_fno_help_lists_only_advertised_menu():
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
    # Names itself as the full (top-level) surface and points at the scoped door.
    assert "full top-level surface" in result.output
    assert "fno help <group> --all" in result.output
    for cmd in _ADVERTISED_SUBCOMMANDS + _HIDDEN_SUBCOMMANDS:
        assert cmd in result.output, f"{cmd!r} missing from fno help --all"


def test_help_group_all_lists_hidden_subverbs():
    """Codex P2: `fno help <group> --all` gives a discovery path for hidden
    nested verbs (e.g. `fno agents ask`, `fno backlog ready`) that neither the
    parent `--help` nor the top-level `help --all` surfaces."""
    import re

    from fno.cli import app
    from typer.testing import CliRunner

    runner = CliRunner()
    for group, hidden_verb in (("agents", "ask"), ("backlog", "ready"), ("mail", "migrate-bus")):
        result = runner.invoke(app, ["help", group, "--all"])
        assert result.exit_code == 0, f"help {group} --all failed: {result.output}"
        plain = _strip_ansi(result.output)
        assert f"fno {group} full command surface" in plain
        assert re.search(rf"^\s*{re.escape(hidden_verb)}\b", plain, re.MULTILINE), (
            f"hidden verb {hidden_verb!r} missing from `fno help {group} --all`"
        )


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

def test_fno_backlog_ready_via_real_subprocess():
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


# ---------------------------------------------------------------------------
# AC4-ERR: the error-reporting path is not itself a lazy tenant
# ---------------------------------------------------------------------------

def test_error_path_never_first_imports_rich_utils():
    """AC4-ERR: typer defers `from . import rich_utils` to exception time in TWO
    places -- typer.main.except_hook (gated by pretty_exceptions_enable) and the
    ClickException arm of typer.core._main (gated by rich_markup_mode). During a
    `uv tool install --reinstall` the package is being replaced under the running
    process, so that first-time import fails too and the operator is shown
    `cannot import name 'rich_utils' from 'typer'` instead of the real cause.
    Setting only one of these leaves the other path live, which is why both are
    asserted here."""
    from fno.cli import app

    assert app.pretty_exceptions_enable is False
    assert app.rich_markup_mode is None


def test_building_the_command_does_not_import_rich_utils():
    """AC4-ERR, the behavioral half: the flags above are only worth their comment if
    typer.rich_utils actually stays unimported through command construction. Run in a
    subprocess so an earlier test's imports cannot mask a regression."""
    code = (
        "import sys, typer.main\n"
        "from fno.cli import app\n"
        "typer.main.get_command(app)\n"
        "print('typer.rich_utils' in sys.modules)\n"
    )
    result = subprocess.run(
        [sys.executable, "-c", code], capture_output=True, text=True, timeout=60
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "False", (
        f"typer.rich_utils became resident during command construction:\n{result.stdout}"
    )


# ---------------------------------------------------------------------------
# AC5-ERR / AC6-EDGE: a missing fno submodule names the reinstall window
# ---------------------------------------------------------------------------

def test_fno_module_import_failure_names_reinstall_window():
    """AC5-ERR: imports happen at invocation time, so `uv tool install --reinstall`
    replaces the package underneath a running process and any not-yet-imported
    subcommand fails for the length of the install. The operator-facing message must
    name both candidate causes (reinstall in flight, stale install) and the fix for
    each, not just the transient one."""
    from fno._lazy_group import make_lazy_group_cls
    import typer
    from typer.testing import CliRunner

    bad_cls = make_lazy_group_cls({"bad": "fno.does_not_exist_module_xyz:cli"})
    # Mirror the real app's error-path config (see fno.cli.app), so this exercises
    # the rendering the operator actually gets.
    test_app = typer.Typer(
        cls=bad_cls,
        no_args_is_help=True,
        pretty_exceptions_enable=False,
        rich_markup_mode=None,
    )

    @test_app.callback()
    def _cb() -> None:
        pass

    result = CliRunner().invoke(test_app, ["bad"])
    assert result.exit_code != 0
    combined = (result.output or "") + (result.stderr if hasattr(result, "stderr") else "")
    # Collapse whitespace: any renderer is free to wrap the message across lines.
    flat = " ".join(combined.replace("│", " ").split())
    assert "reinstalled underneath the running process" in flat, combined
    assert "fno doctor" in flat, combined


def test_third_party_import_failure_has_no_reinstall_hint():
    """AC6-EDGE: a missing third-party dependency is a genuinely broken install. It
    must not collect reinstall speculation, and the prefix test must not match a
    package that merely starts with the letters 'fno'."""
    from fno._lazy_group import _import_failure_hint

    assert _import_failure_hint(ModuleNotFoundError("boom", name="rich")) == ""
    assert _import_failure_hint(ModuleNotFoundError("boom", name="fnord")) == ""
    assert _import_failure_hint(ModuleNotFoundError("boom")) == ""  # name unset
    assert "retry" in _import_failure_hint(
        ModuleNotFoundError("boom", name="fno.graph._reconcile")
    )
    assert "retry" in _import_failure_hint(ModuleNotFoundError("boom", name="fno"))


# ---------------------------------------------------------------------------
# AC8-WIN: verify-then-retry across a tree that changed mid-run
# ---------------------------------------------------------------------------

def test_module_is_now_on_disk_sees_a_file_written_after_the_dir_was_listed(tmp_path):
    """The retry gate must read the PRESENT, not a cached past: a module written
    into an already-imported package must be visible, because that is exactly what
    a reinstall does to a running process.

    Scope note, so this docstring does not overclaim: it passes with or without the
    invalidate_caches() call inside, because FileFinder re-lists a directory whose
    mtime changed and APFS mtimes are fine-grained enough to notice. What it pins is
    the BEHAVIOR the retry depends on, not that one implementation detail."""
    import sys as _sys
    from fno._lazy_group import _module_is_now_on_disk

    pkg = tmp_path / "winpkg"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("", encoding="utf-8")
    _sys.path.insert(0, str(tmp_path))
    try:
        import winpkg  # noqa: F401  (populates the finder cache for pkg/)

        assert _module_is_now_on_disk("winpkg.late") is False
        # Write the module AFTER the directory has been listed and cached.
        (pkg / "late.py").write_text("x = 1\n", encoding="utf-8")
        assert _module_is_now_on_disk("winpkg.late") is True
    finally:
        _sys.path.remove(str(tmp_path))
        _sys.modules.pop("winpkg", None)
        _sys.modules.pop("winpkg.late", None)


def _counting_import(monkeypatch, target: str, results: list):
    """Patch import_module so calls to `target` pop from `results`; count the calls."""
    import importlib as _il
    from fno import _lazy_group as lg

    calls: list[str] = []
    real = _il.import_module

    def fake(name, package=None):
        if name == target:
            calls.append(name)
            outcome = results.pop(0)
            if isinstance(outcome, BaseException):
                raise outcome
            return outcome
        return real(name, package)

    monkeypatch.setattr(lg.importlib, "import_module", fake)
    return calls


def test_lazy_import_retries_once_when_the_tree_changed_underneath(monkeypatch):
    """AC8-WIN: a reinstall can replace the package between process start and the
    lazy import. When the missing module is present again on re-check, the command
    must succeed rather than surface a transient failure to the operator."""
    from fno import _lazy_group as lg
    from fno._lazy_group import make_lazy_group_cls
    import typer
    from typer.testing import CliRunner

    real_target = importlib.import_module("fno.state.cli")
    calls = _counting_import(
        monkeypatch,
        "fno.state.cli",
        [ModuleNotFoundError("gone", name="fno.state._mid_reinstall"), real_target],
    )
    monkeypatch.setattr(lg, "_module_is_now_on_disk", lambda name: True)

    app = typer.Typer(
        cls=make_lazy_group_cls({"state": "fno.state.cli:cli"}),
        no_args_is_help=True,
        pretty_exceptions_enable=False,
        rich_markup_mode=None,
    )

    @app.callback()
    def _cb() -> None:
        pass

    result = CliRunner().invoke(app, ["state", "--help"])
    assert len(calls) == 2, f"expected exactly one retry, got {len(calls)} imports"
    assert result.exit_code == 0, result.output


def test_lazy_import_does_not_retry_when_the_module_is_really_missing(monkeypatch):
    """AC8-WIN, the half that keeps this honest: a genuinely stale or broken install
    answers 'absent' on re-check, so it is NOT retried and still fails with the
    message it fails with today. Without this the retry would be a blind sleep."""
    from fno import _lazy_group as lg
    from fno._lazy_group import make_lazy_group_cls
    import typer
    from typer.testing import CliRunner

    calls = _counting_import(
        monkeypatch,
        "fno.state.cli",
        [ModuleNotFoundError("gone", name="fno.state._really_gone")],
    )
    monkeypatch.setattr(lg, "_module_is_now_on_disk", lambda name: False)

    app = typer.Typer(
        cls=make_lazy_group_cls({"state": "fno.state.cli:cli"}),
        no_args_is_help=True,
        pretty_exceptions_enable=False,
        rich_markup_mode=None,
    )

    @app.callback()
    def _cb() -> None:
        pass

    result = CliRunner().invoke(app, ["state", "--help"])
    assert len(calls) == 1, f"a missing module must not be retried; got {len(calls)}"
    assert result.exit_code != 0
    combined = (result.output or "") + (result.stderr if hasattr(result, "stderr") else "")
    flat = " ".join(combined.replace("│", " ").split())
    assert "fno doctor" in flat, combined


def _run_one_lazy_command(monkeypatch, results):
    """Invoke a lazy 'state' command whose import outcomes come from `results`."""
    from fno._lazy_group import make_lazy_group_cls
    import typer
    from typer.testing import CliRunner

    calls = _counting_import(monkeypatch, "fno.state.cli", results)
    app = typer.Typer(
        cls=make_lazy_group_cls({"state": "fno.state.cli:cli"}),
        no_args_is_help=True,
        pretty_exceptions_enable=False,
        rich_markup_mode=None,
    )

    @app.callback()
    def _cb() -> None:
        pass

    return calls, CliRunner().invoke(app, ["state", "--help"])


def test_plain_import_error_is_never_retried(monkeypatch):
    """A 'cannot import name X from Y' names a module that EXISTS, so the on-disk
    check would say yes and the whole module tree would re-execute for nothing,
    running every import-time side effect twice. Only ModuleNotFoundError proves a
    module was absent, so only it earns a retry."""
    from fno import _lazy_group as lg

    # on_disk deliberately True: the guard must be the exception TYPE, not this.
    monkeypatch.setattr(lg, "_module_is_now_on_disk", lambda name: True)
    boom = ImportError("cannot import name 'gone' from 'fno.config'", name="fno.config")
    calls, result = _run_one_lazy_command(monkeypatch, [boom])

    assert len(calls) == 1, f"a plain ImportError must not be retried; got {len(calls)}"
    assert result.exit_code != 0


def test_retry_failure_is_reported_instead_of_the_stale_first_error(monkeypatch):
    """The retry gets further through the import tree, so it can surface a different
    and more truthful cause. Reporting the original would bury a real missing
    dependency under an fno reinstall hint and send the operator to `fno update` for
    something `fno update` cannot fix."""
    from fno import _lazy_group as lg

    monkeypatch.setattr(lg, "_module_is_now_on_disk", lambda name: True)
    first = ModuleNotFoundError("gone", name="fno.state._mid_reinstall")
    second = ModuleNotFoundError("No module named 'some_third_party'", name="some_third_party")
    calls, result = _run_one_lazy_command(monkeypatch, [first, second])

    assert len(calls) == 2
    assert result.exit_code != 0
    combined = (result.output or "") + (result.stderr if hasattr(result, "stderr") else "")
    flat = " ".join(combined.replace("│", " ").split())
    assert "some_third_party" in flat, combined
    # The stale fno hint must NOT be attached to a third-party failure.
    assert "fno doctor" not in flat, combined
    assert "_mid_reinstall" not in flat, combined


# ---------------------------------------------------------------------------
# x-2304: the meta_path finder, covering the deferred imports written inside
# command bodies (which never route through _load_real)
# ---------------------------------------------------------------------------

def test_the_window_finder_is_armed_by_importing_fno():
    """Arming happens in ``fno/__init__.py``, so every entry into the package
    gets it -- there is no later hook the ~2100 function-level ``from fno. ...``
    imports all pass through.  Pinned as a positive marker (the finder is IN
    ``sys.meta_path``) rather than as the absence of a failure."""
    import sys as _sys

    import fno
    from fno._import_guard import _ReinstallWindowFinder

    armed = [f for f in _sys.meta_path if isinstance(f, _ReinstallWindowFinder)]
    assert len(armed) == 1, _sys.meta_path
    # Last, so it is consulted only after every ordinary finder has missed.
    assert isinstance(_sys.meta_path[-1], _ReinstallWindowFinder)
    # Idempotent: a second import must not stack a second copy.
    importlib.reload(fno)
    assert sum(isinstance(f, _ReinstallWindowFinder) for f in _sys.meta_path) == 1


def test_finder_retries_a_module_that_lands_on_disk_mid_import(tmp_path, monkeypatch):
    """A module absent when the ordinary finders looked, and present a moment
    later, imports -- which is the whole reinstall window.

    The real import machinery runs: ``PathFinder`` genuinely misses (the file
    does not exist yet), the finder re-checks, and the module loads.  Wrapping
    ``_find_spec_now`` is the CLOCK, not the logic under test: it decides the
    instant the reinstall finishes.  Writing the file before the import instead
    would prove nothing, because ``PathFinder`` would find it on the first pass
    and this finder would never be consulted."""
    import fno
    from fno import _import_guard as guard

    monkeypatch.setattr(fno, "__path__", [*fno.__path__, str(tmp_path)])
    real_find = guard._find_spec_now
    rechecks = []

    def finish_the_reinstall(name):
        rechecks.append(name)
        (tmp_path / "late_window.py").write_text("VALUE = 42\n", encoding="utf-8")
        return real_find(name)

    monkeypatch.setattr(guard, "_find_spec_now", finish_the_reinstall)
    try:
        assert importlib.import_module("fno.late_window").VALUE == 42
    finally:
        sys.modules.pop("fno.late_window", None)
    # Exactly once: the re-check is the retry, and there is no second look.
    assert rechecks == ["fno.late_window"]


def test_finder_does_not_retry_an_absent_module_and_names_both_causes(monkeypatch):
    """A genuinely stale install must still fail immediately.

    Two assertions, and the second is the load-bearing one: the disk is
    consulted exactly ONCE, so an absent module is never waited on.  Drop that
    and this becomes the blind sleep-retry that ``cli-lazy-imports.md``
    rejects, which masks a broken install as a transient one."""
    from fno import _import_guard as guard

    real_find = guard._find_spec_now
    rechecks = []

    def spy(name):
        rechecks.append(name)
        return real_find(name)

    monkeypatch.setattr(guard, "_find_spec_now", spy)
    with pytest.raises(ModuleNotFoundError) as excinfo:
        importlib.import_module("fno._x2304_never_written")

    assert rechecks == ["fno._x2304_never_written"]
    message = str(excinfo.value)
    assert "reinstalled underneath the running process" in message
    assert "fno doctor" in message


def test_finder_leaves_a_third_party_module_alone():
    """The finder must not decorate someone else's missing dependency, and must
    not answer for a package that merely starts with the letters ``fno``."""
    for absent in ("rich_x2304_absent", "fnord_x2304_absent"):
        with pytest.raises(ModuleNotFoundError) as excinfo:
            importlib.import_module(absent)
        assert "fno doctor" not in str(excinfo.value)


def test_the_hint_is_stated_once_when_the_finder_already_said_it():
    """``_load_real`` appends the hint to whatever it caught, and what it now
    catches is the finder's own already-hinted error.  Two copies of one
    diagnosis in one line reads as two separate findings."""
    from fno._import_guard import _hint_for_name, _import_failure_hint

    already = ModuleNotFoundError(
        f"No module named 'fno.gone'{_hint_for_name('fno.gone')}", name="fno.gone"
    )
    assert _import_failure_hint(already) == ""
    assert _import_failure_hint(ModuleNotFoundError("boom", name="fno.gone")) != ""
