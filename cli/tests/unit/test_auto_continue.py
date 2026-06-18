"""Tests for config.auto_continue + the enable-resolution resolver.

Covers the merge-triggered auto-continue opt-in (node ab-3cd195b6):

- the ``config.auto_continue.enabled`` schema block (default False), mirroring
  the config.post_merge / config.target.handoff blocks;
- the malformed-block fail-safe (AC2-ERR): a non-boolean ``enabled`` degrades
  to disabled and does NOT raise out of load_settings();
- the ``auto_continue_enabled()`` resolver precedence
  (env override > campaign-arm marker file > settings.yaml > default False).

The resolver is the chokepoint advance() consults before dispatching anything,
so "disabled by default" (AC2-HP) and "malformed fails safe" (AC2-ERR) are the
load-bearing invariants here.
"""
from __future__ import annotations

from pathlib import Path

import pytest


# ---------------------------------------------------------------------------
# Helpers (mirrors test_config_handoff.py)
# ---------------------------------------------------------------------------


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


# ---------------------------------------------------------------------------
# Schema: defaults + round-trip + malformed fail-safe
# ---------------------------------------------------------------------------


def test_auto_continue_default_disabled(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """AC2-HP: with no config.auto_continue block, enabled is False."""
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert settings.config.auto_continue.enabled is False


def test_auto_continue_enabled_true_roundtrip(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    settings = _load(
        tmp_path,
        monkeypatch,
        "config:\n  auto_continue:\n    enabled: true\n",
    )
    assert settings.config.auto_continue.enabled is True


def test_auto_continue_malformed_block_fails_safe(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-ERR: a non-boolean enabled degrades to disabled, never raises."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "config:\n  auto_continue:\n    enabled: not-a-bool\n",
    )
    assert settings.config.auto_continue.enabled is False


def test_auto_continue_malformed_whole_block_fails_safe(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-ERR: a scalar where the block should be a mapping degrades safely."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "config:\n  auto_continue: 42\n",
    )
    assert settings.config.auto_continue.enabled is False


# ---------------------------------------------------------------------------
# Resolver: env > marker file > settings > default
# ---------------------------------------------------------------------------


def _resolver():
    from fno.backlog.advance import auto_continue_enabled

    return auto_continue_enabled


def test_resolver_default_false(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("FNO_AUTO_CONTINUE", raising=False)
    _load(tmp_path, monkeypatch, "schema_version: 1\n")
    assert _resolver()(project_root=tmp_path) is False


def test_resolver_settings_true(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("FNO_AUTO_CONTINUE", raising=False)
    _load(tmp_path, monkeypatch, "config:\n  auto_continue:\n    enabled: true\n")
    assert _resolver()(project_root=tmp_path) is True


def test_resolver_env_override_true(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Env override wins even when settings say disabled (highest precedence)."""
    _load(tmp_path, monkeypatch, "schema_version: 1\n")
    monkeypatch.setenv("FNO_AUTO_CONTINUE", "1")
    assert _resolver()(project_root=tmp_path) is True


def test_resolver_env_override_false(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Env override can also force-disable even when settings enable."""
    _load(tmp_path, monkeypatch, "config:\n  auto_continue:\n    enabled: true\n")
    monkeypatch.setenv("FNO_AUTO_CONTINUE", "0")
    assert _resolver()(project_root=tmp_path) is False


def test_resolver_marker_file_arms(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A campaign-arm marker file enables even with default settings."""
    monkeypatch.delenv("FNO_AUTO_CONTINUE", raising=False)
    _load(tmp_path, monkeypatch, "schema_version: 1\n")
    marker = tmp_path / ".fno" / ".auto-continue-armed"
    marker.parent.mkdir(parents=True, exist_ok=True)
    marker.write_text("", encoding="utf-8")
    assert _resolver()(project_root=tmp_path) is True


def test_resolver_never_raises_on_bad_settings(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-ERR: resolver swallows any load failure and returns False."""
    monkeypatch.delenv("FNO_AUTO_CONTINUE", raising=False)
    # Point FNO_CONFIG at a path that is a directory -> load raises.
    bad = tmp_path / "not-a-file"
    bad.mkdir()
    monkeypatch.setenv("FNO_CONFIG", str(bad))
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    assert _resolver()(project_root=tmp_path) is False
