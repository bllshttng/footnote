"""Tests for config.target.handoff - the self-handoff succession config block.

Covers defaults, field validation, out-of-range rejection, and yaml round-trip
through load_settings. The shell consumer (skills/target/scripts/handoff.sh)
reads the same keys via get_config "target.handoff.*" with matching defaults
(50/4/true) -- both sides must stay in sync.

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
    """With no config.target.handoff block: enabled=True, used_pct_trigger=50, generation_cap=4.

    Defaults MUST match the shell defaults in handoff.sh (see the get_config
    calls at ~line 100-102 of that file).
    """
    settings = _load(tmp_path, monkeypatch, "schema_version: 1\n")
    handoff = settings.config.target.handoff
    assert handoff.enabled is True
    assert handoff.used_pct_trigger == 50
    assert handoff.generation_cap == 4


def test_handoff_override_all_fields(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """All three fields can be overridden via settings.yaml."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  target:\n    handoff:\n"
        "      enabled: false\n"
        "      used_pct_trigger: 75\n"
        "      generation_cap: 2\n",
    )
    handoff = settings.config.target.handoff
    assert handoff.enabled is False
    assert handoff.used_pct_trigger == 75
    assert handoff.generation_cap == 2


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
        assert settings.config.target.handoff.used_pct_trigger == val


def test_handoff_generation_cap_rejects_zero(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """generation_cap=0 is rejected (must be >= 1)."""
    with pytest.raises(Exception, match=r"generation_cap|>=\s*1|at least 1"):
        _load(
            tmp_path,
            monkeypatch,
            "schema_version: 1\nconfig:\n  target:\n    handoff:\n"
            "      generation_cap: 0\n",
        )


def test_handoff_generation_cap_accepts_one(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """generation_cap=1 is the minimum valid value."""
    settings = _load(
        tmp_path,
        monkeypatch,
        "schema_version: 1\nconfig:\n  target:\n    handoff:\n"
        "      generation_cap: 1\n",
    )
    assert settings.config.target.handoff.generation_cap == 1


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
    assert result.output.strip() == "50"


def test_config_get_generation_cap_default(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """config get config.target.handoff.generation_cap returns 4 when unset."""
    result = _config_get(
        tmp_path, monkeypatch, "config.target.handoff.generation_cap", "schema_version: 1\n"
    )
    assert result.exit_code == 0, result.output
    assert result.output.strip() == "4"


def test_config_get_enabled_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """config get config.target.handoff.enabled returns True when unset."""
    result = _config_get(
        tmp_path, monkeypatch, "config.target.handoff.enabled", "schema_version: 1\n"
    )
    assert result.exit_code == 0, result.output
    assert result.output.strip() == "True"


def test_config_get_enabled_override(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """config get reflects enabled: false override."""
    result = _config_get(
        tmp_path,
        monkeypatch,
        "config.target.handoff.enabled",
        "schema_version: 1\nconfig:\n  target:\n    handoff:\n      enabled: false\n",
    )
    assert result.exit_code == 0, result.output
    assert result.output.strip() == "False"


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
    assert result.output.strip() == "80"
