"""Tests for fno.paths_testing fixture helper.

Task 4.6 (Phase 4): use_tmpdir isolates paths correctly.

AC4-HP: paths.X() resolves under tmp_path after use_tmpdir
AC4-EDGE: calling use_tmpdir twice clears the cache between calls
AC4-EDGE: no real ~/.fno state is read after use_tmpdir

Autouse fixture pins FNO_REPO_ROOT per feedback_fno_repo_root_leaks_between_tests.
"""
from __future__ import annotations

from pathlib import Path
from typing import Generator

import pytest


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    """Pin FNO_REPO_ROOT and clear caches before/after each test."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    paths_mod._settings.cache_clear()
    yield
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()


def test_use_tmpdir_isolates_graph_json(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC4-HP: graph_json() resolves inside tmp_path after use_tmpdir."""
    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)

    from fno.paths import graph_json
    result = graph_json()
    assert str(result).startswith(str(tmp_path)), (
        f"Expected path under {tmp_path}, got {result}"
    )


def test_use_tmpdir_isolates_state_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC4-HP: state_dir() resolves inside tmp_path after use_tmpdir."""
    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)

    from fno.paths import state_dir
    result = state_dir()
    assert str(result).startswith(str(tmp_path)), (
        f"Expected path under {tmp_path}, got {result}"
    )


def test_use_tmpdir_returns_settings_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC4-HP: use_tmpdir returns the settings.yaml path."""
    from fno.paths_testing import use_tmpdir
    settings_path = use_tmpdir(monkeypatch, tmp_path)

    assert settings_path.exists(), "settings.yaml must be created"
    assert settings_path.name == "settings.yaml"


def test_use_tmpdir_does_not_touch_real_home(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC4-HP: no path resolves to real ~/.fno/ after use_tmpdir."""
    real_home_fno = Path.home() / ".fno"

    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)

    from fno.paths import graph_json, ledger_json, state_dir
    for accessor in (state_dir, graph_json, ledger_json):
        result = accessor()
        assert not str(result).startswith(str(real_home_fno)), (
            f"{accessor.__name__}() returned path under real ~/.fno: {result}"
        )


def test_use_tmpdir_second_call_overrides_first(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC4-EDGE: calling use_tmpdir twice uses the second call's settings."""
    from fno.paths_testing import use_tmpdir

    first_settings = use_tmpdir(monkeypatch, tmp_path)
    # Patch settings to something distinctive, then call again
    second_dir = tmp_path / "second"
    second_dir.mkdir()
    second_settings = use_tmpdir(monkeypatch, second_dir)

    from fno.paths import state_dir
    result = state_dir()
    # Should be under second_dir
    assert str(result).startswith(str(second_dir)), (
        f"Expected path under {second_dir}, got {result}"
    )
    # First settings path is still the original tmp_path structure
    assert first_settings != second_settings


def test_use_tmpdir_clears_cache_between_calls(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC4-EDGE: cache is cleared so second call takes effect immediately."""
    from fno.paths_testing import use_tmpdir
    import fno.paths as paths_mod

    use_tmpdir(monkeypatch, tmp_path)

    # After first call, settings are loaded
    from fno.paths import graph_json
    first_result = graph_json()

    # Modify the settings file directly and call use_tmpdir again
    second_dir = tmp_path / "second-state"
    second_dir.mkdir()
    use_tmpdir(monkeypatch, second_dir)

    # Cache was cleared, new settings take effect
    # Re-import to avoid stale reference
    from fno.paths import graph_json as graph_json2
    second_result = graph_json2()

    assert str(second_result).startswith(str(second_dir)), (
        f"Expected path under {second_dir}, got {second_result}; "
        f"cache was not cleared between calls"
    )
