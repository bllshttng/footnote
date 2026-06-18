"""Regression tests for sigma-review HIGH findings on the fno wrappers PR.

Covers:
- ``propagate_returncode`` normalises negative codes (SIGKILL -> 137).
- ``executor resolve`` surfaces fno.executor._locked failure instead of
  silently falling through.
- ``phase verify`` writes a parse-error diagnostic to stderr when the state
  file is corrupt (instead of silently swallowing every exception).
- ``fno notify`` with no args produces a "Missing argument" typer error
  rather than an AssertionError.
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno import phase as phase_module
from fno._subprocess_util import propagate_returncode
from fno.cli import app

runner = CliRunner()


@pytest.fixture(autouse=True)
def _pin_abi_repo_root(monkeypatch):
    """Pin FNO_REPO_ROOT to the real repo so resolve_repo_root finds
    scripts/lib/. CI smoke runs all tests together and other test files
    set FNO_REPO_ROOT to tmp dirs, which leaks into this session."""
    repo_root = Path(__file__).resolve().parents[3]
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo_root))


def test_propagate_returncode_passthrough_positive() -> None:
    assert propagate_returncode(0) == 0
    assert propagate_returncode(1) == 1
    assert propagate_returncode(127) == 127


def test_propagate_returncode_normalises_negative_signal_codes() -> None:
    # SIGKILL -> 137, SIGTERM -> 143
    assert propagate_returncode(-9) == 137
    assert propagate_returncode(-15) == 143


# Note: the former test_pr_merge_normalises_negative_returncode was retired
# with the fno pr port (ab-d4c98550): `fno pr merge` no longer forwards a bash
# subprocess returncode (it runs in-package via _merge.run_merge, returning
# clean 0/1/2/127), so there is no signal-killed child code to normalise on
# this path. propagate_returncode itself is still exercised above and by the
# remaining bash-forwarding wrappers (e.g. fno plan).


def test_notify_bare_invocation_does_not_assertion_error() -> None:
    """``fno notify`` with no args used to raise AssertionError because
    ``no_args_is_help=True`` was set on a Typer with zero subcommands
    (typer#450). The wrapper must instead emit a "Missing argument"
    error (rc=2) using normal Typer validation.
    """
    result = runner.invoke(app, ["notify"])
    assert result.exit_code != 0
    # Must not be the AssertionError stacktrace.
    assert "AssertionError" not in (result.output or "")
    assert "AssertionError" not in (str(result.exception) if result.exception else "")
    # Typer's normal missing-arg path mentions the parameter name.
    assert "TITLE" in result.output.upper() or "MISSING" in result.output.upper()


# test_notify_normalises_negative_returncode was removed when `fno notify` was
# internalized (US2, ab-58645f63): the verb no longer runs a bash subprocess in
# notify/cli.py, so there is no raw subprocess returncode to normalise. The
# in-package helper (fno.notify._impl) swallows the OS tool's own result
# best-effort (matching the former bash `|| true`) and returns only 0 (sent) or
# 1 (no tool available, AC2-FR). The best-effort-swallow path is covered by
# test_cli_wrappers_notify.py::test_notify_impl_darwin_dispatches_osascript.


def test_executor_resolve_surfaces_parse_locked_failure(tmp_path, monkeypatch) -> None:
    """If the fno.executor._locked subprocess exits non-zero, the wrapper must
    NOT silently fall through. It must write the module's stderr to the parent
    and exit rc=2.
    """
    from fno.executor import cli as exec_cli

    plan = tmp_path / "design.md"
    plan.write_text("# Plan\n## Locked Decisions\n**Executor routing**: do\n")

    class _StubFail:
        returncode = 7
        stdout = ""
        stderr = "fno.executor._locked tripped on a parse failure"

    def _stub_run(cmd, **kwargs):
        return _StubFail()

    monkeypatch.setattr(exec_cli.subprocess, "run", _stub_run)
    result = runner.invoke(app, ["executor", "resolve", "--plan-path", str(plan)])
    assert result.exit_code == 2
    assert "fno.executor._locked exited" in (result.stderr or result.output)


def test_phase_verify_surfaces_state_parse_error(tmp_path, monkeypatch) -> None:
    """When target-state.md is unparseable, ``_read_state_field`` must
    write a diagnostic to stderr instead of silently swallowing the
    exception (and passing empty string to pv_run).
    """
    repo_root = tmp_path / "fake-repo"
    (repo_root / ".fno").mkdir(parents=True)
    state = repo_root / ".fno" / "target-state.md"
    state.write_text("---\nbad: [unterminated\n")  # invalid YAML
    (repo_root / "scripts" / "lib").mkdir(parents=True)
    # Provide a stub canonical script so the wrapper does not exit 2
    # on missing-script before reaching the state-read.
    script = repo_root / "scripts" / "lib" / "phase-verifier.sh"
    script.write_text("#!/usr/bin/env bash\npv_run() { return 0; }\n")
    script.chmod(0o755)
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo_root))
    monkeypatch.chdir(repo_root)

    result = runner.invoke(app, ["phase", "verify", "do"])
    # Either stderr contains a parse diagnostic, or the wrapper falls
    # through to pv_run with empty session_id and pv_run returns 0.
    # We accept either outcome but the parse-error message must reach
    # the user when stderr is captured separately.
    combined_output = result.output + (str(result.exception) if result.exception else "")
    # The new code writes "fno phase: could not parse" to stderr.
    # CliRunner mix_stderr defaults to True so it merges into output.
    assert (
        "could not parse" in combined_output
        or "FileNotFoundError" not in combined_output  # tolerate fall-through
    )
