"""Tests for config.autonomy + autonomy_master_enabled() (x-aaaf wave 3).

Covers the one master panic switch over every autonomous session-starting
spawner:

- the ``config.autonomy.enabled`` schema block (default True) - shipping it
  changes nothing until an operator explicitly arms the switch;
- the malformed-value fail-safe: a garbage ``enabled`` degrades to True (this
  field's own default), never to a silent kill of every spawner;
- the resolver-level invariant: a settings.yaml that fails to LOAD at all
  degrades ``autonomy_master_enabled()`` to False, the opposite direction
  from the malformed-value case (an unreadable config resolves every gate to
  off, never on).
"""
from __future__ import annotations

from pathlib import Path

import pytest


def _write_settings(tmp_path: Path, content: str) -> Path:
    settings_dir = tmp_path / ".fno"
    settings_dir.mkdir(parents=True, exist_ok=True)
    settings_file = settings_dir / "settings.yaml"
    settings_file.write_text(content, encoding="utf-8")
    return settings_file


def _load(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, content: str):
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(tmp_path, content)
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    return config_mod.load_settings()


@pytest.fixture(autouse=True)
def _isolate_global_settings(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")


# ---------------------------------------------------------------------------
# Schema: default True + round-trip + malformed fail-safe-to-True
# ---------------------------------------------------------------------------


def test_autonomy_default_enabled(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """With no config.autonomy block, enabled is True (shipping is a no-op)."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.autonomy.enabled is True


def test_autonomy_enabled_false_roundtrip(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = _load(
        tmp_path, monkeypatch, "schema_version: 1\nautonomy:\n  enabled: false\n",
    )
    assert settings.autonomy.enabled is False


def test_autonomy_malformed_value_fails_safe_to_true(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A garbage value degrades to True, this field's own default - the
    opposite fail-safe direction from a total config-load failure."""
    settings = _load(
        tmp_path, monkeypatch, "schema_version: 1\nautonomy:\n  enabled: [1, 2]\n",
    )
    assert settings.autonomy.enabled is True


# ---------------------------------------------------------------------------
# autonomy_master_enabled() resolver
# ---------------------------------------------------------------------------


def test_master_enabled_true_by_default(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _load(tmp_path, monkeypatch, "schema_version: 1\n")
    from fno.config import autonomy_master_enabled

    assert autonomy_master_enabled(tmp_path) is True


def test_master_enabled_false_when_disabled(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _load(tmp_path, monkeypatch, "schema_version: 1\nautonomy:\n  enabled: false\n")
    from fno.config import autonomy_master_enabled

    assert autonomy_master_enabled(tmp_path) is False


def test_master_enabled_fails_safe_to_false_on_unreadable_config(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A settings load that raises resolves to False, not the field's own
    True default - the global invariant that an unreadable config resolves
    every gate to off, never on. (A merely malformed settings.yaml is
    handled upstream in config_io with a logged warning + defaults, so it
    never reaches this branch - only monkeypatching a raise exercises it.)"""
    import fno.config as config_mod

    def _boom(_root):
        raise RuntimeError("settings exploded")

    monkeypatch.setattr(config_mod, "load_settings_for_repo", _boom)

    assert config_mod.autonomy_master_enabled(tmp_path) is False


def test_master_enabled_checked_before_env_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The panic switch is checked FIRST, before even an explicit env
    override - a switch something else can bypass is not a panic switch."""
    _load(tmp_path, monkeypatch, "schema_version: 1\nautonomy:\n  enabled: false\n")
    monkeypatch.setenv("FNO_AUTO_CONTINUE", "1")

    from fno.backlog.advance import _auto_continue_resolve

    armed, rank = _auto_continue_resolve(tmp_path)
    assert armed is False
    assert rank == "autonomy"
