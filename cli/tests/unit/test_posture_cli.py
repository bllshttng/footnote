"""Tests for `fno posture apply` (x-8cd5 Wave 6) - a generator, never a layer.

Posture writes real keys through the shared atomic `fno config set` writer and
exits; afterwards there is one source of truth and no posture-specific
resolution path. These tests pin: refusal of an unknown posture before any
write, that each stance writes its keys, that an advisory provenance stamp
lands in a side file, and that the keys read back as ordinary config.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.config import load_settings_for_repo
from fno.posture_cli import POSTURE_KEYS, posture_app

runner = CliRunner()


@pytest.fixture
def project_scope(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Isolate --local (project-scope) config writes into a tmp repo root."""
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda: tmp_path)
    return tmp_path


def test_apply_refuses_unknown_posture(project_scope: Path) -> None:
    res = runner.invoke(posture_app, ["apply", "bogus", "--local"])
    assert res.exit_code == 2
    assert "unknown posture" in (res.output + (res.stderr or "")).lower()
    # Refused BEFORE any write, so nothing landed.
    assert not (project_scope / ".fno" / "config.toml").exists()


def test_apply_attended_writes_off_keys(project_scope: Path) -> None:
    res = runner.invoke(posture_app, ["apply", "attended", "--local"])
    assert res.exit_code == 0, res.output
    s = load_settings_for_repo(project_scope)
    assert s.auto_merge.enabled is False
    assert s.dispatch.auto_merge is False
    assert s.active_backlog.enabled is False


def test_apply_autonomous_writes_on_keys(project_scope: Path) -> None:
    res = runner.invoke(posture_app, ["apply", "autonomous", "--local"])
    assert res.exit_code == 0, res.output
    s = load_settings_for_repo(project_scope)
    assert s.auto_merge.enabled is True
    assert s.dispatch.auto_merge is True
    assert s.active_backlog.enabled is True


def test_apply_writes_provenance_stamp(
    project_scope: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("fno.paths.state_dir", lambda: project_scope / ".fno")
    res = runner.invoke(posture_app, ["apply", "autonomous", "--local"])
    assert res.exit_code == 0, res.output
    stamp = json.loads((project_scope / ".fno" / "posture.json").read_text())
    assert stamp["posture"] == "autonomous"
    assert stamp["scope"] == "project"
    assert stamp["keys"] == sorted(POSTURE_KEYS["autonomous"])


def test_posture_keys_read_back_as_ordinary_config(project_scope: Path) -> None:
    """No posture-specific resolution path: applied keys load via the normal
    loader, and there is no `posture` field on the model to consult."""
    runner.invoke(posture_app, ["apply", "attended", "--local"])
    s = load_settings_for_repo(project_scope)
    assert not hasattr(s, "posture")
    assert s.auto_merge.enabled is False
