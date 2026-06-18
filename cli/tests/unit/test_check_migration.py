"""Tests for _check_migration() in fno.cli.

Finding D (P2): _check_migration() must pass settings_root=_paths.state_dir()
to run_migration(). Without this, sentinel check happens at the configured
state_dir but migration writes to ~/.fno, so the sentinel is never seen
at the active path and migration re-fires on every invocation.
"""
from __future__ import annotations

from pathlib import Path
from typing import Generator
from unittest.mock import patch, MagicMock

import pytest


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    """Clear caches and suppress auto-migration before each test."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_SKIP_MIGRATION", "1")
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(config_mod, "_loaded_from"):
        config_mod._loaded_from = None
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass
    if hasattr(paths_mod, "resolve_repo_root"):
        try:
            paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass
    yield
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(config_mod, "_loaded_from"):
        config_mod._loaded_from = None
    import fno.paths as paths_mod2
    if hasattr(paths_mod2, "_settings"):
        try:
            paths_mod2._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass


# ---------------------------------------------------------------------------
# AC-D-HP: _check_migration passes active state_dir to run_migration
# ---------------------------------------------------------------------------


def test_check_migration_passes_state_dir_to_run_migration(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC-D-HP: _check_migration() passes settings_root=_paths.state_dir() to run_migration.

    Finding D: without this, sentinel check uses active state_dir but migration
    writes to ~.fno (hardcoded default), so migration re-fires forever
    when state_dir != ~/.fno.

    Strategy: configure a custom state_dir, suppress FNO_SKIP_MIGRATION so
    _check_migration() runs, mock run_migration, verify it's called with the
    custom state_dir as settings_root.
    """
    custom_state = tmp_path / "custom-abi"
    custom_state.mkdir()

    # Write a settings.yaml that sets a custom state_dir
    abilities_dir = tmp_path / ".fno"
    abilities_dir.mkdir()
    settings_file = abilities_dir / "settings.yaml"
    settings_file.write_text(
        f"schema_version: 1\nconfig:\n  state_dir: '{custom_state}'\n",
        encoding="utf-8",
    )

    # Point FNO_CONFIG to the settings file so paths.state_dir() sees it
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))
    monkeypatch.delenv("FNO_SKIP_MIGRATION", raising=False)
    monkeypatch.delenv("PYTEST_CURRENT_TEST", raising=False)

    # Clear caches so the new config is picked up
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(config_mod, "_loaded_from"):
        config_mod._loaded_from = None
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass

    # No sentinel at the custom state_dir - migration should fire
    assert not (custom_state / ".path-migration-done").exists()

    captured_calls: list[dict] = []

    def _mock_run_migration(**kwargs: object) -> int:
        captured_calls.append(dict(kwargs))
        return 0

    with patch("fno.setup.migrate_paths.run_migration", side_effect=_mock_run_migration):
        from fno.cli import _check_migration
        _check_migration()

    assert captured_calls, "_check_migration must call run_migration (no sentinel present)"
    call_kwargs = captured_calls[0]
    assert "settings_root" in call_kwargs, (
        f"run_migration must be called with settings_root kwarg; got: {call_kwargs}"
    )
    assert call_kwargs["settings_root"] == custom_state, (
        f"settings_root must equal _paths.state_dir() ({custom_state!s}); "
        f"got: {call_kwargs['settings_root']}"
    )


# ---------------------------------------------------------------------------
# Finding F (P2): stale tmp cleanup must be inside lock (run_migration), not in
# _check_migration before lock acquisition
# ---------------------------------------------------------------------------


def test_check_migration_does_not_delete_tmp_outside_lock(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Finding F (P2): _check_migration must not delete .tmp files before acquiring the lock.

    Deleting stale tmps outside the lock creates a race: process A cleans stale
    tmps, then process B deletes A's in-flight tmp, then A's os.replace() fails.

    The fix: remove the pre-lock glob-unlink from _check_migration; rely on
    run_migration's inside-lock cleanup only.

    Strategy: create a fake .tmp file in the settings directory, run
    _check_migration (with run_migration mocked), verify the .tmp file is NOT
    deleted by _check_migration itself (it's only safe to delete inside the lock
    in run_migration).
    """
    custom_state = tmp_path / "state-f"
    custom_state.mkdir()

    abilities_dir = tmp_path / ".fno"
    abilities_dir.mkdir()
    settings_file = abilities_dir / "settings.yaml"
    settings_file.write_text(
        f"schema_version: 1\nconfig:\n  state_dir: '{custom_state}'\n",
        encoding="utf-8",
    )

    monkeypatch.setenv("FNO_CONFIG", str(settings_file))
    monkeypatch.delenv("FNO_SKIP_MIGRATION", raising=False)
    monkeypatch.delenv("PYTEST_CURRENT_TEST", raising=False)

    # Clear caches
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(config_mod, "_loaded_from"):
        config_mod._loaded_from = None
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass

    # Create a fake "in-flight" .tmp file as if another process is writing it
    # Put it next to the settings.yaml that config_file() will return
    inflight_tmp = abilities_dir / ".settings.yaml.FAKEPID.tmp"
    inflight_tmp.write_text("inflight content", encoding="utf-8")
    assert inflight_tmp.exists(), "setup: inflight tmp must exist"

    # Mock run_migration to do nothing (we want to test _check_migration's own behavior)
    from unittest.mock import patch

    with patch("fno.setup.migrate_paths.run_migration", return_value=0):
        from fno.cli import _check_migration
        _check_migration()

    # The inflight tmp must NOT have been deleted by _check_migration outside the lock
    assert inflight_tmp.exists(), (
        "_check_migration must not delete .tmp files outside the lock; "
        "stale cleanup belongs inside run_migration (which holds the lock)"
    )
