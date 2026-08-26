"""Tests for capability escalation and shared context-compact thresholds.

Covers defaults, field validation, out-of-range rejection, and yaml round-trip
through load_settings. The shell consumer (skills/target/scripts/handoff.sh)
reads the enabled key while context-nudge owns the percentage thresholds.

Node: ab-534bcc55. Locked Decisions 6-8.
"""
from __future__ import annotations

from pathlib import Path

import pytest


# ---------------------------------------------------------------------------
# Helpers (mirrors test_config_post_merge.py pattern)
# ---------------------------------------------------------------------------


def _write_settings(tmp_path: Path, content: str) -> Path:
    """Write a settings.yaml to tmp_path/.fno/ and return the path."""
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


def _config_get(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, key: str, content: str):
    """Invoke `fno config get <key>` in-process against the source schema."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    settings_file = _write_settings(tmp_path, content)
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    from fno.cli import app
    from typer.testing import CliRunner

    return CliRunner().invoke(app, ["config", "get", key])


# ---------------------------------------------------------------------------
# AC1-HP: Schema defaults resolve correctly
# ---------------------------------------------------------------------------


def test_handoff_defaults(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The block keeps escalation enabled and the compact threshold at 50.

    Defaults MUST match the shell defaults in handoff.sh (see the get_config
    calls at ~line 100-102 of that file).
    """
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    handoff = settings.target.handoff
    assert handoff.enabled is True
    assert handoff.used_pct_trigger == 50
    assert not hasattr(handoff, "generation_cap")


def test_handoff_override_live_fields(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Live fields override, while a legacy generation cap is ignored."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  target:\n    handoff:\n"
        "      enabled: false\n"
        "      used_pct_trigger: 75\n"
        "      generation_cap: 2\n",
    )
    handoff = settings.target.handoff
    assert handoff.enabled is False
    assert handoff.used_pct_trigger == 75
    assert not hasattr(handoff, "generation_cap")


# ---------------------------------------------------------------------------
# AC2-ERR: Out-of-range values are rejected at load time
# ---------------------------------------------------------------------------


def test_handoff_used_pct_trigger_rejects_zero(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """used_pct_trigger=0 is rejected (must be 1-100)."""
    with pytest.raises(Exception, match=r"used_pct_trigger|1.*100|range"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  target:\n    handoff:\n"
            "      used_pct_trigger: 0\n",
        )


def test_handoff_used_pct_trigger_rejects_over_100(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """used_pct_trigger=101 is rejected (must be 1-100)."""
    with pytest.raises(Exception, match=r"used_pct_trigger|1.*100|range"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  target:\n    handoff:\n"
            "      used_pct_trigger: 101\n",
        )


def test_handoff_used_pct_trigger_boundary_values_accepted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """used_pct_trigger accepts boundary values 1 and 100."""
    for val in (1, 100):
        settings = _load(
            tmp_path,
            monkeypatch,
            f"schema_version: 1\nconfig:\n  target:\n    handoff:\n"
            f"      used_pct_trigger: {val}\n",
        )
        assert settings.target.handoff.used_pct_trigger == val


def test_handoff_legacy_generation_cap_is_ignored(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Old config remains loadable but cannot create extra escalation rungs."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  target:\n    handoff:\n"
        "      generation_cap: 99\n",
    )
    assert not hasattr(settings.target.handoff, "generation_cap")


# ---------------------------------------------------------------------------
# AC3-VERIFY: yaml round-trip via `fno config get`
# ---------------------------------------------------------------------------


def test_config_get_used_pct_trigger_default(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """config get config.target.handoff.used_pct_trigger returns 50 when unset."""
    result = _config_get(
        tmp_path, monkeypatch, "config.target.handoff.used_pct_trigger", "schema_version: 1\n"
    )
    assert result.exit_code == 0, result.output
    assert result.stdout.strip() == "50"


def test_config_get_generation_cap_default(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The retired generation key is no longer a public config surface."""
    result = _config_get(
        tmp_path, monkeypatch, "config.target.handoff.generation_cap", "schema_version: 1\n"
    )
    assert result.exit_code != 0
    assert "unknown config key" in result.output


def test_config_get_enabled_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """config get config.target.handoff.enabled returns True when unset."""
    result = _config_get(
        tmp_path, monkeypatch, "config.target.handoff.enabled", "schema_version: 1\n"
    )
    assert result.exit_code == 0, result.output
    assert result.stdout.strip() == "True"


def test_config_get_enabled_override(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """config get reflects enabled: false override."""
    result = _config_get(
        tmp_path,
        monkeypatch,
        "config.target.handoff.enabled",
        "schema_version: 1\nconfig:\n  target:\n    handoff:\n      enabled: false\n",
    )
    assert result.exit_code == 0, result.output
    assert result.stdout.strip() == "False"


def test_config_get_used_pct_trigger_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """config get reflects a custom used_pct_trigger."""
    result = _config_get(
        tmp_path,
        monkeypatch,
        "config.target.handoff.used_pct_trigger",
        "schema_version: 1\nconfig:\n  target:\n    handoff:\n      used_pct_trigger: 80\n",
    )
    assert result.exit_code == 0, result.output
    assert result.stdout.strip() == "80"
