"""Tests for the 'fno runtime probe' subcommand."""
from __future__ import annotations

import json
import shutil
import sys
from pathlib import Path
from unittest.mock import patch

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


def _run_probe(extra_args: list[str] | None = None) -> object:
    args = ["runtime", "probe"]
    if extra_args:
        args.extend(extra_args)
    return runner.invoke(app, args)


# AC1-HP: probe returns green when all prereqs present
def test_ac1_hp_probe_green_all_present(tmp_path, monkeypatch):
    """probe returns exit 0 and ok=true JSON when all checks pass."""
    # Create a fake plugin.json in a temp dir, point probe there
    plugin_json = tmp_path / "plugin.json"
    plugin_json.write_text('{"name": "fno"}')

    # Simulate all tools being present
    with patch("shutil.which", return_value="/usr/local/bin/tool"):
        with patch("subprocess.run") as mock_run:
            mock_run.return_value.returncode = 0
            mock_run.return_value.stdout = "gh version 2.0.0"
            result = _run_probe(["--plugin-path", str(plugin_json), "--json"])

    assert result.exit_code == 0, f"Expected exit 0, got {result.exit_code}. Output: {result.output}"
    data = json.loads(result.output)
    assert data["ok"] is True
    assert "checks" in data
    check_names = [c["name"] for c in data["checks"]]
    assert "git" in check_names
    assert "gh-auth" in check_names
    assert "python" in check_names
    assert "plugin.json" in check_names


def test_ac1_hp_probe_checks_have_pass_field(tmp_path):
    """Each check in the response has 'name' and 'pass' fields."""
    plugin_json = tmp_path / "plugin.json"
    plugin_json.write_text('{"name": "fno"}')

    with patch("shutil.which", return_value="/usr/local/bin/tool"):
        with patch("subprocess.run") as mock_run:
            mock_run.return_value.returncode = 0
            mock_run.return_value.stdout = "gh version 2.0.0"
            result = _run_probe(["--plugin-path", str(plugin_json), "--json"])

    data = json.loads(result.output)
    for check in data["checks"]:
        assert "name" in check
        assert "pass" in check


# AC2-ERR: probe returns red when gh missing
def test_ac2_err_probe_red_gh_missing(tmp_path):
    """probe returns exit 4 when gh is not on PATH."""
    plugin_json = tmp_path / "plugin.json"
    plugin_json.write_text('{"name": "fno"}')

    def fake_which(cmd):
        if cmd == "gh":
            return None
        return "/usr/bin/" + cmd

    with patch("shutil.which", side_effect=fake_which):
        result = _run_probe(["--plugin-path", str(plugin_json), "--json"])

    assert result.exit_code == 4, f"Expected exit 4, got {result.exit_code}. Output: {result.output}"
    data = json.loads(result.output)
    assert data["ok"] is False
    # Find gh-auth check
    gh_check = next((c for c in data["checks"] if c["name"] == "gh-auth"), None)
    assert gh_check is not None
    assert gh_check["pass"] is False
    assert "note" in gh_check
    assert "brew install gh" in gh_check["note"] or "cli.github.com" in gh_check["note"]


def test_ac2_err_probe_red_git_missing(tmp_path):
    """probe returns exit 4 when git is not on PATH."""
    plugin_json = tmp_path / "plugin.json"
    plugin_json.write_text('{"name": "fno"}')

    def fake_which(cmd):
        if cmd == "git":
            return None
        return "/usr/bin/" + cmd

    with patch("shutil.which", side_effect=fake_which):
        result = _run_probe(["--plugin-path", str(plugin_json), "--json"])

    assert result.exit_code == 4
    data = json.loads(result.output)
    assert data["ok"] is False


def test_ac2_err_probe_missing_plugin_json(tmp_path):
    """probe returns exit 4 when plugin.json does not exist."""
    missing = tmp_path / "nonexistent" / "plugin.json"

    with patch("shutil.which", return_value="/usr/bin/tool"):
        with patch("subprocess.run") as mock_run:
            mock_run.return_value.returncode = 0
            result = _run_probe(["--plugin-path", str(missing), "--json"])

    assert result.exit_code == 4
    data = json.loads(result.output)
    plugin_check = next((c for c in data["checks"] if c["name"] == "plugin.json"), None)
    assert plugin_check is not None
    assert plugin_check["pass"] is False
