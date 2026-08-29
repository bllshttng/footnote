"""Tests for _default_settings_path in fno.adapters.providers.dispatch.

Finding 5 (P2): project-local settings file must be checked BEFORE resolving
global config_file(), so that a ValidationError from global settings doesn't
prevent using the project-local snapshot.
"""
from __future__ import annotations

import os
from pathlib import Path
from typing import Generator

import pytest


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    """Clear caches before each test."""
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
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
    config_mod._loaded_from = None
    if hasattr(paths_mod, "_settings"):
        try:
            paths_mod._settings.cache_clear()  # type: ignore[attr-defined]
        except AttributeError:
            pass


# ---------------------------------------------------------------------------
# AC5-HP: project_local exists -> return it without calling config_file()
# ---------------------------------------------------------------------------


def test_default_settings_path_returns_project_local_when_exists(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC5-HP: When project-local settings.yaml exists, return it directly.

    Finding 5: project_local.is_file() must be checked BEFORE config_file()
    is called, so validation errors from global settings don't surface here.
    """
    # Create a project-local settings.yaml at cwd/.fno/settings.yaml
    cwd = tmp_path / "my-project"
    cwd.mkdir()
    fno_dir = cwd / ".fno"
    fno_dir.mkdir()
    local_settings = fno_dir / "config.toml"
    local_settings.write_text("schema_version = 1\n", encoding="utf-8")

    # Override PWD so _default_settings_path uses our test dir
    monkeypatch.setenv("PWD", str(cwd))

    from fno.adapters.providers.dispatch import _default_settings_path
    result = _default_settings_path()
    assert result == local_settings, (
        f"Expected project-local path {local_settings}, got {result}"
    )


def test_default_settings_path_falls_back_to_config_file_when_no_local(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC5-HP: When no project-local settings.yaml, fallback to config_file()."""
    cwd = tmp_path / "no-local"
    cwd.mkdir()
    # No .fno/settings.yaml here
    monkeypatch.setenv("PWD", str(cwd))

    # Write a global settings.yaml and point FNO_CONFIG at it
    global_settings = tmp_path / "global-settings.yaml"
    global_settings.write_text("schema_version = 1\n", encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(global_settings))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    config_mod._loaded_from = None
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

    from fno.adapters.providers.dispatch import _default_settings_path
    result = _default_settings_path()
    # Should fall back to config_file() which returns the loaded path
    assert result == global_settings.resolve(), (
        f"Expected global settings {global_settings}, got {result}"
    )


def test_default_settings_path_project_local_returned_even_if_config_file_raises(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC5-ERR: Finding 5 - project_local must be checked BEFORE config_file().

    If global settings.yaml is malformed (ValidationError), calling config_file()
    would raise. But if a project-local settings.yaml exists, we should return it
    without ever calling config_file(). This test verifies the ordering guarantee.
    """
    cwd = tmp_path / "has-local"
    cwd.mkdir()
    local_settings = cwd / ".fno" / "config.toml"
    local_settings.parent.mkdir()
    local_settings.write_text("schema_version = 1\n", encoding="utf-8")
    monkeypatch.setenv("PWD", str(cwd))

    # Write a global settings that would fail with glob chars (causes ValidationError)
    # Point FNO_CONFIG at it
    bad_settings = tmp_path / "bad-global.yaml"
    bad_settings.write_text("schema_version: 1\nconfig:\n  state_dir: '~/.fno/*'\n", encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(bad_settings))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    config_mod._loaded_from = None
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

    from fno.adapters.providers.dispatch import _default_settings_path

    # Must NOT raise, even though config_file() would trigger a ValidationError
    result = _default_settings_path()
    assert result == local_settings, (
        f"Expected project-local {local_settings}, got {result}"
    )


def test_default_settings_path_project_local_wins_over_global(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC5-HP: project-local checked FIRST, even when global settings exist.

    This is the key ordering guarantee from Finding 5.
    """
    cwd = tmp_path / "has-local"
    cwd.mkdir()
    local_settings = cwd / ".fno" / "config.toml"
    local_settings.parent.mkdir()
    local_settings.write_text("schema_version = 1\n", encoding="utf-8")
    monkeypatch.setenv("PWD", str(cwd))

    # Also have a global settings (via FNO_CONFIG)
    global_settings = tmp_path / "global.yaml"
    global_settings.write_text("schema_version = 1\n", encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(global_settings))

    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    config_mod._loaded_from = None
    import fno.paths as paths_mod
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

    from fno.adapters.providers.dispatch import _default_settings_path
    result = _default_settings_path()
    # Project-local must win
    assert result == local_settings, (
        f"Expected project-local {local_settings} to win over global, got {result}"
    )


# ---------------------------------------------------------------------------
# x-5cc5: the restore seam re-resolves the provider-DEFAULT tier. The helper
# is unit-tested in test_model_routing.py; THIS test pins the call site, so a
# merge that resolves toward the old verbatim-replay body goes red instead of
# silently regressing to stale-tier resumes.
# ---------------------------------------------------------------------------


def test_restore_route_for_relaunch_refreshes_a_stale_default_tier(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    import json
    from types import SimpleNamespace

    # The autouse _isolate fixture points FNO_REPO_ROOT at an empty tmp_path
    # and clears the load_settings cache, so the seam resolves the BUILT-IN
    # provider registry (zai haiku glm-4.7), not this machine's config.
    from fno.agents.model_routing import DEFAULT_ZAI_BASE_URL

    recorded = tmp_path / "route.json"
    recorded.write_text(
        json.dumps({
            "env": {
                "ANTHROPIC_BASE_URL": DEFAULT_ZAI_BASE_URL,
                "ANTHROPIC_AUTH_TOKEN": "zk-secret",
                "ANTHROPIC_MODEL": "glm-5.3",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.5-air",
                "FNO_ROUTE_PROVIDER": "zai",
            }
        }),
        encoding="utf-8",
    )

    from fno.agents.dispatch import restore_route_for_relaunch

    restored = restore_route_for_relaunch(
        SimpleNamespace(route_settings_path=str(recorded), name="row-x")
    )

    assert restored is not None
    assert restored["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "glm-4.7"
    # Operator-chosen keys replay verbatim through the seam.
    assert restored["ANTHROPIC_AUTH_TOKEN"] == "zk-secret"
    assert restored["ANTHROPIC_MODEL"] == "glm-5.3"
    err = capsys.readouterr().err
    assert "glm-4.5-air" in err and "glm-4.7" in err and "zai" in err


def test_restore_route_for_relaunch_returns_none_without_a_recorded_path() -> None:
    from types import SimpleNamespace

    from fno.agents.dispatch import restore_route_for_relaunch

    assert restore_route_for_relaunch(SimpleNamespace(route_settings_path=None, name="row-y")) is None
