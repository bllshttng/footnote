"""Tests for _load_v2_config_flag in fno.cli.

Finding 6 (P2): _load_v2_config_flag must fail open (return False) even when
paths.config_file() or settings loading raises ValidationError or other errors.
"""
from __future__ import annotations

from pathlib import Path
from typing import Generator

import pytest


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    """Clear caches before each test."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    config_mod._loaded_from = None
    yield
    config_mod._loaded_from = None
# ---------------------------------------------------------------------------
# AC6-HP: Normal case - no v2_enabled, returns False
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# AC6-ERR: Fails open (returns False) on settings resolution errors
# ---------------------------------------------------------------------------


def test_load_v2_config_flag_fails_open_on_validation_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC6-ERR: Finding 6 - returns False even when config_file() raises ValidationError.

    A malformed global settings.yaml (glob chars in state_dir) would cause
    config_file() to raise ValidationError. _load_v2_config_flag must catch
    this and return False, not propagate the exception.
    """
    # Write a settings.yaml with glob chars that will cause ValidationError
    bad_settings = tmp_path / "bad-global.yaml"
    bad_settings.write_text(
        "schema_version: 1\nconfig:\n  state_dir: '~/.fno/*'\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CONFIG", str(bad_settings))

    from fno import config as config_mod
    config_mod._loaded_from = None

    from fno.cli import _load_v2_config_flag

    # Must return False, not raise
    result = _load_v2_config_flag(tmp_path)
    assert result is False, (
        "Expected _load_v2_config_flag to return False on ValidationError, not propagate"
    )
