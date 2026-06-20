"""Tests for `fno config unset` / fno.config.writer.unset_config_value (x-50f9, US1).

Covers AC1-HP (removes key, reverts to default, file no longer contains it),
AC1-ERR (unknown key exits 1, file unchanged), AC1-UI (CLI confirm line),
AC1-EDGE (absent key is a clean no-op; emptied parent pruned),
AC1-FR (mid-write failure leaves the original intact).
"""
from __future__ import annotations

import pytest
import yaml
from typer.testing import CliRunner

from fno.config.writer import ConfigSetError, set_config_value, unset_config_value


def _read(tmp_path):
    return yaml.safe_load((tmp_path / ".fno" / "settings.yaml").read_text())


def test_ac1_hp_unset_removes_key_reverts_to_default(tmp_path):
    set_config_value(
        "config.auto_merge.enabled", "true", scope="project", repo_root=tmp_path
    )
    assert _read(tmp_path)["config"]["auto_merge"]["enabled"] is True

    res = unset_config_value(
        "config.auto_merge.enabled", scope="project", repo_root=tmp_path
    )
    assert res.present is True
    assert res.was is True
    # Reverts to the model default (auto_merge.enabled defaults False).
    assert res.default is False
    # File no longer carries the key.
    data = _read(tmp_path)
    assert "enabled" not in data.get("config", {}).get("auto_merge", {})


def test_ac1_err_unknown_key_exit1_unchanged(tmp_path):
    set_config_value(
        "config.agents.a2a.auto", "false", scope="project", repo_root=tmp_path
    )
    before = (tmp_path / ".fno" / "settings.yaml").read_text()
    with pytest.raises(ConfigSetError) as exc:
        unset_config_value("config.nonsense.key", scope="project", repo_root=tmp_path)
    assert exc.value.exit_code == 1
    assert (tmp_path / ".fno" / "settings.yaml").read_text() == before


def test_ac1_edge_absent_key_is_noop(tmp_path):
    # Seed a different key so the file exists.
    set_config_value(
        "config.agents.a2a.auto", "false", scope="project", repo_root=tmp_path
    )
    before = (tmp_path / ".fno" / "settings.yaml").read_text()
    res = unset_config_value(
        "config.auto_merge.enabled", scope="project", repo_root=tmp_path
    )
    assert res.present is False
    # The seeded key (and the rest of the file) survive.
    assert _read(tmp_path)["config"]["agents"]["a2a"]["auto"] is False
    # Nothing meaningful changed (modulo a harmless reserialize is acceptable,
    # but an absent-key unset on an existing file should not drop other keys).
    assert "a2a" in _read(tmp_path)["config"]["agents"]


def test_ac1_edge_no_file_is_clean_noop(tmp_path):
    # No settings file at all: unset is a clean no-op, writes nothing.
    res = unset_config_value(
        "config.auto_merge.enabled", scope="project", repo_root=tmp_path
    )
    assert res.present is False
    assert not (tmp_path / ".fno" / "settings.yaml").exists()


def test_ac1_edge_emptied_parent_is_pruned(tmp_path):
    # Only one leaf under config.auto_merge: removing it should prune the block.
    set_config_value(
        "config.auto_merge.enabled", "true", scope="project", repo_root=tmp_path
    )
    data = _read(tmp_path)
    assert list(data["config"]["auto_merge"].keys()) == ["enabled"]

    unset_config_value(
        "config.auto_merge.enabled", scope="project", repo_root=tmp_path
    )
    data = _read(tmp_path)
    # The now-empty auto_merge block is pruned, not left as `{}`.
    assert "auto_merge" not in data.get("config", {})


def test_unset_preserves_sibling_keys(tmp_path):
    set_config_value(
        "config.agents.a2a.auto", "false", scope="project", repo_root=tmp_path
    )
    set_config_value(
        "config.agents.a2a.turn_ceiling", "9", scope="project", repo_root=tmp_path
    )
    unset_config_value(
        "config.agents.a2a.auto", scope="project", repo_root=tmp_path
    )
    data = _read(tmp_path)
    # The sibling under the same block survives; the block is NOT pruned.
    assert data["config"]["agents"]["a2a"]["turn_ceiling"] == 9
    assert "auto" not in data["config"]["agents"]["a2a"]


def test_ac1_fr_midwrite_failure_leaves_intact(tmp_path, monkeypatch):
    set_config_value(
        "config.auto_merge.enabled", "true", scope="project", repo_root=tmp_path
    )
    before = (tmp_path / ".fno" / "settings.yaml").read_text()

    import fno.config.writer as writer_mod

    def _boom(*a, **k):
        raise OSError("disk full")

    monkeypatch.setattr(writer_mod.yaml, "safe_dump", _boom)
    with pytest.raises(ConfigSetError) as exc:
        unset_config_value(
            "config.auto_merge.enabled", scope="project", repo_root=tmp_path
        )
    assert exc.value.exit_code == 1
    assert (tmp_path / ".fno" / "settings.yaml").read_text() == before
    leftovers = list((tmp_path / ".fno").glob(".settings.yaml.tmp.*"))
    assert leftovers == []


def test_ac1_ui_cli_confirms_unset(tmp_path, monkeypatch):
    gpath = tmp_path / "global-settings.yaml"
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(gpath))
    from fno.config_cli import app

    runner = CliRunner()
    runner.invoke(app, ["set", "config.auto_merge.enabled", "true"])
    res = runner.invoke(app, ["unset", "config.auto_merge.enabled"])
    assert res.exit_code == 0, res.output
    assert "unset" in res.output
    assert "config.auto_merge.enabled" in res.output
    assert "defaults to" in res.output
    assert "global" in res.output


def test_cli_unset_absent_key_reports_not_set(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "g.yaml"))
    from fno.config_cli import app

    res = CliRunner().invoke(app, ["unset", "config.auto_merge.enabled"])
    assert res.exit_code == 0
    assert "not set" in res.output


def test_cli_rm_alias_unsets(tmp_path, monkeypatch):
    gpath = tmp_path / "g.yaml"
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(gpath))
    from fno.config_cli import app

    runner = CliRunner()
    runner.invoke(app, ["set", "config.auto_merge.enabled", "true"])
    res = runner.invoke(app, ["rm", "config.auto_merge.enabled"])
    assert res.exit_code == 0, res.output
    assert "unset" in res.output
    data = yaml.safe_load(gpath.read_text())
    assert "enabled" not in data.get("config", {}).get("auto_merge", {})
