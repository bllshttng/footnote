"""Tests for capability escalation and shared context-compact thresholds.

Covers live-field overrides and out-of-range rejection through load_settings.
The shell consumer (skills/target/scripts/handoff.sh) reads the enabled key
while context-nudge owns the percentage thresholds.

Node: ab-534bcc55. Locked Decisions 6-8.
"""
from __future__ import annotations

from pathlib import Path

import pytest


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

    # The declaration key is unchanged by a content rewrite; drop the entry.
    config_mod._load_settings_at.cache_clear()
    return config_mod.load_settings()


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
