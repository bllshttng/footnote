"""CLI-layer tests for `fno pr verify` and `fno pr rebase` after the
in-package port (ab-d4c98550).

The verify / rebase logic itself is characterized in test_pr_verify.py and
test_pr_rebase.py. Here we only assert the Typer verbs dispatch into the
in-package modules, propagate their exit codes, and keep their option
validation (required --pr-number/--state-file, the kind enum). The old
"forwards to verify-*.sh / rebase-resolve.sh" assertions are retired.
"""
from __future__ import annotations

from typer.testing import CliRunner

from fno.cli import app
from fno.pr import _verify

runner = CliRunner()


# ---------------------------------------------------------------------------
# fno pr verify
# ---------------------------------------------------------------------------


def test_verify_help_renders():
    result = runner.invoke(app, ["pr", "verify", "--help"])
    assert result.exit_code == 0


def test_verify_kind_dispatches_in_package(monkeypatch):
    """--kind merged -> run_verify_merged; --kind reviews -> run_verify_reviews."""
    calls = {}

    def _merged(pr, sf, cwd=None, **kw):
        calls["merged"] = (pr, sf)
        return 0

    def _reviews(pr, sf, cwd=None, **kw):
        calls["reviews"] = (pr, sf)
        return 0

    monkeypatch.setattr(_verify, "run_verify_merged", _merged)
    monkeypatch.setattr(_verify, "run_verify_reviews", _reviews)

    r1 = runner.invoke(app, ["pr", "verify", "--kind", "merged", "--pr-number", "42", "--state-file", "x.md"])
    assert r1.exit_code == 0
    assert calls["merged"] == ("42", "x.md")

    r2 = runner.invoke(app, ["pr", "verify", "--kind", "reviews", "--pr-number", "7", "--state-file", "y.md"])
    assert r2.exit_code == 0
    assert calls["reviews"] == ("7", "y.md")


def test_verify_exit_code_propagates(monkeypatch):
    monkeypatch.setattr(_verify, "run_verify_merged", lambda *a, **k: 1)
    result = runner.invoke(
        app, ["pr", "verify", "--kind", "merged", "--pr-number", "1", "--state-file", "x.md"]
    )
    assert result.exit_code == 1


def test_verify_invalid_kind_rejected():
    """--kind bogus exits rc=2 and names valid values."""
    result = runner.invoke(
        app, ["pr", "verify", "--kind", "bogus", "--pr-number", "1", "--state-file", "x.md"]
    )
    assert result.exit_code == 2
    assert "merged" in result.output and "reviews" in result.output


def test_verify_missing_pr_number_yields_exit_2():
    result = runner.invoke(app, ["pr", "verify", "--kind", "merged", "--state-file", "x.md"])
    assert result.exit_code == 2


def test_verify_missing_state_file_yields_exit_2():
    result = runner.invoke(app, ["pr", "verify", "--kind", "merged", "--pr-number", "1"])
    assert result.exit_code == 2


# ---------------------------------------------------------------------------
# fno pr rebase: contract is characterized against real git repos in
# test_pr_rebase.py; only the help surface is asserted here.
# ---------------------------------------------------------------------------


def test_rebase_help_renders():
    result = runner.invoke(app, ["pr", "rebase", "--help"])
    assert result.exit_code == 0
