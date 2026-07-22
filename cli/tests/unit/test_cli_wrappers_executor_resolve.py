"""Unit tests for `fno executor resolve` three-tier wrapper.

AC-HP (tier 1): plan with Locked Decisions + Executor routing -> locked value
AC-HP (tier 2): no lock + tsx task files -> impeccable
AC-HP (tier 3): no args -> do
AC-EDGE-1 (--explain): stdout includes tier trace
AC-ERR: missing --plan-path -> rc=2; resolution is repo-root-independent
AC-UI: --help documents all options
"""
from __future__ import annotations

from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture(autouse=True)
def _pin_fno_repo_root(monkeypatch):
    """Pin FNO_REPO_ROOT to the real repo so resolve_repo_root finds
    scripts/lib/. Other tests in the suite (e.g. test_gate_honesty,
    test_gates) set FNO_REPO_ROOT to tmp dirs and that env-var can leak
    into pytest sessions where this file runs after them. The CI smoke
    job exposed this ordering dependency at PR-open time."""
    repo_root = Path(__file__).resolve().parents[3]
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo_root))


# Wide-COLUMNS env so typer doesn't column-wrap help in narrow CI TTYs
# (memory: feedback_typer_help_wraps_at_narrow_ci_width).
_HELP_ENV = {"COLUMNS": "240", "NO_COLOR": "1", "TERM": "dumb"}


# ---------------------------------------------------------------------------
# AC-UI: help renders with documented options
# ---------------------------------------------------------------------------

def test_executor_resolve_help_renders():
    result = runner.invoke(app, ["executor", "resolve", "--help"], env=_HELP_ENV)
    assert result.exit_code == 0
    assert "--plan-path" in result.stdout
    assert "--task-files" in result.stdout
    assert "--explain" in result.stdout


# ---------------------------------------------------------------------------
# AC-HP tier 1: locked decision in plan
# ---------------------------------------------------------------------------

def test_locked_decision_do(tmp_path):
    plan = tmp_path / "design.md"
    plan.write_text(
        "# Some Plan\n\n"
        "## Locked Decisions\n\n"
        "**Executor routing**: plan-level `executor: do`\n\n"
        "## Other Section\n\n"
        "Some prose.\n"
    )
    result = runner.invoke(app, ["executor", "resolve", "--plan-path", str(plan)])
    assert result.exit_code == 0
    assert result.stdout.strip() == "do"


def test_locked_decision_impeccable(tmp_path):
    plan = tmp_path / "design.md"
    plan.write_text(
        "# Some Plan\n\n"
        "## Locked Decisions\n\n"
        "**Executor routing**: plan-level `executor: impeccable`\n\n"
    )
    result = runner.invoke(app, ["executor", "resolve", "--plan-path", str(plan)])
    assert result.exit_code == 0
    assert result.stdout.strip() == "impeccable"


# ---------------------------------------------------------------------------
# AC-HP tier 2: surface inference from file list
# ---------------------------------------------------------------------------

def test_inference_impeccable_from_tsx():
    result = runner.invoke(
        app, ["executor", "resolve", "--task-files", "src/components/Foo.tsx"]
    )
    assert result.exit_code == 0
    assert result.stdout.strip() == "impeccable"


def test_inference_do_from_python():
    result = runner.invoke(
        app, ["executor", "resolve", "--task-files", "cli/loop.py"]
    )
    assert result.exit_code == 0
    assert result.stdout.strip() == "do"


# ---------------------------------------------------------------------------
# AC-HP tier 3: default
# ---------------------------------------------------------------------------

def test_default_do():
    result = runner.invoke(app, ["executor", "resolve"])
    assert result.exit_code == 0
    assert result.stdout.strip() == "do"


# ---------------------------------------------------------------------------
# AC-EDGE-1: --explain shows which tier resolved
# ---------------------------------------------------------------------------

def test_explain_shows_tier_locked(tmp_path):
    plan = tmp_path / "design.md"
    plan.write_text(
        "## Locked Decisions\n\n**Executor routing**: plan-level `executor: do`\n\n"
    )
    result = runner.invoke(
        app, ["executor", "resolve", "--plan-path", str(plan), "--explain"]
    )
    assert result.exit_code == 0
    assert "tier:" in result.stdout
    assert "locked" in result.stdout
    assert "do" in result.stdout


def test_explain_shows_tier_inference():
    result = runner.invoke(
        app,
        ["executor", "resolve", "--task-files", "src/components/Foo.tsx", "--explain"],
    )
    assert result.exit_code == 0
    assert "tier:" in result.stdout
    assert "inference" in result.stdout
    assert "impeccable" in result.stdout


def test_explain_shows_tier_default():
    result = runner.invoke(app, ["executor", "resolve", "--explain"])
    assert result.exit_code == 0
    assert "tier:" in result.stdout
    assert "default" in result.stdout
    assert "do" in result.stdout


# ---------------------------------------------------------------------------
# AC-ERR: error conditions
# ---------------------------------------------------------------------------

def test_missing_plan_path_yields_exit_2(tmp_path):
    missing = tmp_path / "nonexistent.md"
    result = runner.invoke(
        app, ["executor", "resolve", "--plan-path", str(missing)]
    )
    assert result.exit_code == 2
    # Error message should mention the missing file
    combined = result.stdout + (result.stderr or "")
    assert str(missing) in combined or "nonexistent" in combined


def test_resolve_independent_of_repo_root(tmp_path, monkeypatch):
    """The parser is now the in-package module ``fno.executor._locked``, so
    resolution no longer depends on ``FNO_REPO_ROOT`` pointing at a checkout
    with ``scripts/lib/``. Even a fake repo root resolves the lock correctly.

    (Previously this asserted a rc=2 'canonical script not found' path; that
    path is gone now that the parser is always importable - ab-58645f63.)
    """
    fake_root = tmp_path / "fake-repo"
    fake_root.mkdir()
    monkeypatch.setenv("FNO_REPO_ROOT", str(fake_root))

    plan = tmp_path / "design.md"
    plan.write_text("## Locked Decisions\n\n**Executor routing**: plan-level `executor: do`\n\n")

    result = runner.invoke(
        app, ["executor", "resolve", "--plan-path", str(plan)]
    )
    assert result.exit_code == 0
    assert result.stdout.strip() == "do"


# ---------------------------------------------------------------------------
# AC-ERR: mixed lock falls through to inference tier
# ---------------------------------------------------------------------------

def test_mixed_lock_falls_through_to_inference(tmp_path):
    """When the plan says 'mixed', fall through to surface inference."""
    plan = tmp_path / "design.md"
    plan.write_text(
        "## Locked Decisions\n\n**Executor routing**: plan-level `executor: mixed`\n\n"
    )
    result = runner.invoke(
        app,
        [
            "executor", "resolve",
            "--plan-path", str(plan),
            "--task-files", "src/components/Foo.tsx",
        ],
    )
    assert result.exit_code == 0
    assert result.stdout.strip() == "impeccable"
