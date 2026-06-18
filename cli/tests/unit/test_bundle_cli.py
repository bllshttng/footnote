"""Unit tests for `fno bundle` thin wrappers.

Each subcommand is a forwarder. Tests verify:
1. Help text renders (top-level + each subcommand).
2. No-subcommand default runs the bundler (scripts/generate-skill-bundles.sh).
3. ``check`` and ``lint`` subcommands forward to their canonical scripts.
4. Exit codes propagate from the canonical script.
5. Missing canonical script -> diagnostic + exit 2.
"""
from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app


REPO_ROOT = Path(__file__).resolve().parents[3]
runner = CliRunner()


def test_bundle_help_renders():
    """`fno bundle --help` lists check + lint subcommands and explains the
    no-subcommand default action."""
    result = runner.invoke(app, ["bundle", "--help"])
    assert result.exit_code == 0
    assert "check" in result.stdout
    assert "lint" in result.stdout
    assert "Default action" in result.stdout or "regenerates" in result.stdout


def test_bundle_check_help_renders():
    result = runner.invoke(app, ["bundle", "check", "--help"])
    assert result.exit_code == 0
    assert "freshness" in result.stdout


def test_bundle_lint_help_renders():
    result = runner.invoke(app, ["bundle", "lint", "--help"])
    assert result.exit_code == 0
    assert "marketplace" in result.stdout.lower()


def test_bundle_no_subcommand_runs_bundler_against_committed_tree():
    """The no-subcommand default invokes scripts/generate-skill-bundles.sh.
    Run against the real repo tree (idempotent; safe to call from CI)."""
    env = dict(os.environ)
    env["FNO_REPO_ROOT"] = str(REPO_ROOT)
    # Use subprocess directly so we exercise the real `fno bundle` entry
    # point (CliRunner doesn't enter the subprocess that the wrapper spawns).
    result = subprocess.run(
        ["uv", "run", "fno", "bundle"],
        cwd=str(REPO_ROOT / "cli"),
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert "bundled:" in result.stdout


def test_bundle_check_passes_for_fresh_tree():
    """`fno bundle check` exits 0 when the committed bundles match canonical."""
    env = dict(os.environ)
    env["FNO_REPO_ROOT"] = str(REPO_ROOT)
    result = subprocess.run(
        ["uv", "run", "fno", "bundle", "check"],
        cwd=str(REPO_ROOT / "cli"),
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert "skill bundles fresh" in result.stdout


def test_bundle_lint_passes_for_current_state():
    """`fno bundle lint` exits 0 against the current driver-skill state."""
    env = dict(os.environ)
    env["FNO_REPO_ROOT"] = str(REPO_ROOT)
    result = subprocess.run(
        ["uv", "run", "fno", "bundle", "lint"],
        cwd=str(REPO_ROOT / "cli"),
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert "self-contained" in result.stdout


def test_bundle_missing_canonical_script_reports_diagnostic(tmp_path, monkeypatch):
    """When the canonical script can't be found (broken install), the
    wrapper exits 2 with an actionable diagnostic, not a Python traceback."""
    # Point FNO_REPO_ROOT at an empty dir so the canonical scripts are missing.
    fake_root = tmp_path / "fake-repo"
    fake_root.mkdir()
    monkeypatch.setenv("FNO_REPO_ROOT", str(fake_root))

    result = runner.invoke(app, ["bundle", "check"])
    assert result.exit_code == 2
    # AC3-ERR: capability-accurate degrade names the footnote plugin and the
    # bare-install gap + an install path, not "is the plugin installed correctly?".
    out = result.stdout + (result.stderr or "")
    assert "footnote plugin" in out
    assert "pip install fno" in out
    assert "--plugin-dir" in out
