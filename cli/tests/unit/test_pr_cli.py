"""CLI-layer tests for `fno pr merge` after the in-package port (ab-d4c98550).

The merge logic itself is characterized in test_pr_merge.py; here we only
assert the Typer verb dispatches into the in-package _merge module and
propagates its exit code (the old "forwards to pr-merge.sh" assertions are
retired - the bash is gone).
"""
from __future__ import annotations

from typer.testing import CliRunner

from fno.cli import app
from fno.pr import _merge

runner = CliRunner()


def test_pr_help_renders():
    result = runner.invoke(app, ["pr", "--help"])
    assert result.exit_code == 0
    assert "merge" in result.stdout


def test_pr_merge_help_renders():
    result = runner.invoke(app, ["pr", "merge", "--help"])
    assert result.exit_code == 0


def test_pr_merge_dispatches_in_package(monkeypatch):
    """The verb calls _merge.run_merge with the forwarded args and exits its rc."""
    captured = {}

    def _fake(argv, cwd=None):
        captured["argv"] = list(argv)
        return 2

    monkeypatch.setattr(_merge, "run_merge", _fake)
    result = runner.invoke(app, ["pr", "merge", "--invoker=megawalk", "999999"])
    assert result.exit_code == 2
    assert captured["argv"] == ["--invoker=megawalk", "999999"]


def test_pr_merge_invalid_invoker_exit_1():
    """End-to-end through the verb: a bad invoker is rejected with exit 1."""
    result = runner.invoke(app, ["pr", "merge", "--invoker=evil", "42"])
    assert result.exit_code == 1
