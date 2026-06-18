"""Tests for fno.paths.evals_history (Task 1.1).

Covers:
- AC1-HP: default path resolves to <state_dir>/evals-history.jsonl
- AC2-EDGE: config override wins over the default
"""
from __future__ import annotations

from pathlib import Path
from typing import Generator

import pytest


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    paths_mod._settings.cache_clear()
    yield
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()


def test_evals_history_default(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP: no override -> <state_dir>/evals-history.jsonl."""
    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)

    import fno.paths as paths_mod
    result = paths_mod.evals_history()
    assert result == tmp_path / ".fno" / "evals-history.jsonl"


def test_evals_history_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-EDGE: config.paths.evals_history override wins over the default."""
    from fno.paths_testing import use_tmpdir
    from fno import config as config_mod
    import fno.paths as paths_mod

    settings_path = use_tmpdir(monkeypatch, tmp_path)
    custom = tmp_path / "custom" / "eval-runs.jsonl"
    settings_path.write_text(
        f"schema_version: 1\n"
        f"config:\n"
        f"  state_dir: {tmp_path}/.fno/\n"
        f"  paths:\n"
        f"    evals_history: {str(custom)}\n",
        encoding="utf-8",
    )
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()

    result = paths_mod.evals_history()
    assert result == custom
