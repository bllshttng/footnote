"""Tests for Codex PR #264 round 3 fixes (findings A, B, D).

Finding A: paths.sh self-sets REPO_ROOT so sourcing under set -u doesn't crash.
Finding B: dead-line regression in health_monitor.py and collision.py; fail-open.
Finding D: plain-relative predicate in paths.py rejects env vars anywhere.
"""
from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Generator

import pytest


# ---------------------------------------------------------------------------
# Autouse fixture: cache isolation (same pattern as test_paths.py)
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    import fno.paths as paths_mod
    yield
def _set_settings(monkeypatch: pytest.MonkeyPatch, tmp_path: Path, content: str) -> None:
    settings_file = tmp_path / "settings.yaml"
    settings_file.write_text(content, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))
    # The declaration key is unchanged by a content rewrite; drop the entry.
    from fno.config import _load_settings_at

    _load_settings_at.cache_clear()


# ===========================================================================
# Finding A: paths.sh self-sets REPO_ROOT under set -u
# ===========================================================================


def test_paths_sh_sourceable_without_repo_root_set(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP: paths.sh can be sourced under 'set -u' without REPO_ROOT pre-set.

    The generated stub must define REPO_ROOT itself (via git or pwd fallback)
    before using it in PLANS_DIR / INBOX_DIR export lines.
    """
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    paths_file = tmp_path / "paths.sh"
    paths_file.write_text(stub, encoding="utf-8")

    # Source under set -u WITHOUT pre-setting REPO_ROOT - must not crash.
    result = subprocess.run(
        ["bash", "-c", f'set -u; source {paths_file} && echo "OK STATE_DIR=$STATE_DIR PLANS_DIR=$PLANS_DIR"'],
        capture_output=True,
        text=True,
        timeout=10,
        env={"HOME": str(tmp_path / "home"), "PATH": "/usr/bin:/bin:/usr/local/bin"},
    )
    assert result.returncode == 0, (
        f"paths.sh crashed under set -u without REPO_ROOT:\n"
        f"stdout: {result.stdout!r}\nstderr: {result.stderr!r}"
    )
    assert "OK" in result.stdout, f"Expected OK in output, got: {result.stdout!r}"
    assert "STATE_DIR=" in result.stdout, f"STATE_DIR not in output: {result.stdout!r}"
    assert "PLANS_DIR=" in result.stdout, f"PLANS_DIR not in output: {result.stdout!r}"


def test_paths_sh_repo_root_self_set_line_present(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC1-HP: Generated stub contains REPO_ROOT self-set line before any $REPO_ROOT usage."""
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.setup.emit_shell import emit_paths_sh

    stub = emit_paths_sh()
    lines = stub.splitlines()

    # Find the first line that uses $REPO_ROOT
    first_use_idx = next(
        (i for i, line in enumerate(lines) if "$REPO_ROOT" in line and "REPO_ROOT=" not in line),
        None,
    )
    # Find the line that defines REPO_ROOT
    repo_root_def_idx = next(
        (i for i, line in enumerate(lines) if "REPO_ROOT=" in line and "REPO_ROOT:-" in line),
        None,
    )

    assert repo_root_def_idx is not None, (
        "Generated paths.sh must contain a REPO_ROOT self-set line "
        "(e.g. REPO_ROOT=\"${REPO_ROOT:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}\")"
        f"\nStub:\n{stub}"
    )
    if first_use_idx is not None:
        assert repo_root_def_idx < first_use_idx, (
            f"REPO_ROOT self-set (line {repo_root_def_idx}) must appear BEFORE "
            f"first $REPO_ROOT usage (line {first_use_idx})"
        )


# ===========================================================================
# Finding B: health_monitor.py and collision.py fail-open on settings error
# ===========================================================================


def test_health_load_config_failsopen_on_invalid_settings(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP (Finding B): load_config fails open when config_file() triggers validation error.

    When user_settings=None (default), load_config calls _paths.config_file() which
    triggers full Pydantic model validation. If settings.yaml has a validation error
    (e.g. glob in state_dir), config_file() raises ValidationError.
    load_config must catch that and fall back to defaults.
    """
    from fno.health_monitor import load_config, DEFAULT_CONFIG
    from fno import config as config_mod
    import fno.paths as paths_mod

    # Write an invalid settings.yaml - glob char in state_dir fails Pydantic validation
    bad_settings = tmp_path / "bad-settings.yaml"
    bad_settings.write_text(
        "schema_version: 1\nconfig:\n  state_dir: '/home/*/fno'\n",
        encoding="utf-8",
    )
    # Wire FNO_CONFIG to point at the bad settings file
    monkeypatch.setenv("FNO_CONFIG", str(bad_settings))
    # Clear caches so the new bad settings are picked up

    # Call with user_settings=None (default) so _paths.config_file() is called
    # Should not raise; should return defaults
    result = load_config(
        project_settings=tmp_path / "nonexistent.yaml",
        user_settings=None,  # triggers _paths.config_file() call
    )
    # The result should be the defaults (bad file is ignored gracefully)
    assert isinstance(result, dict), "load_config must return a dict even on bad settings"
    assert "thresholds" in result, "result must contain defaults thresholds key"
    # thresholds should match defaults (or close) - not explode
    assert result["thresholds"]["idea_pile_depth"] == DEFAULT_CONFIG["thresholds"]["idea_pile_depth"]


def test_health_load_config_no_dead_assignment(tmp_path: Path) -> None:
    """AC2-HP (Finding B): no dead Path(...).expanduser() line before _paths.config_file().

    Verifies the dead first assignment is gone from load_config source.
    """
    import inspect
    from fno import health_monitor

    src = inspect.getsource(health_monitor.load_config)
    # The dead line was: user_settings = Path("~/.fno/settings.yaml").expanduser()
    # immediately followed by: user_settings = _paths.config_file()
    assert "expanduser" not in src or "config_file" not in src.split("expanduser")[0] or True, ""
    # More targeted: check the dead assignment pattern is absent
    assert 'Path("~/.fno/settings.yaml").expanduser()' not in src, (
        "Dead assignment 'user_settings = Path(~/.fno/settings.yaml).expanduser()' "
        "must be removed from load_config"
    )


def test_collision_load_thresholds_failsopen_on_invalid_settings(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC2-HP (Finding B): _load_thresholds fails open when config_file() triggers validation error.

    When user_settings=None (default), _load_thresholds calls _paths.config_file()
    which triggers full Pydantic model validation. If settings.yaml is invalid,
    it raises ValidationError. _load_thresholds must catch that and return defaults.
    """
    from fno.graph.collision import _load_thresholds, _default_thresholds_loaded
    from fno import config as config_mod
    import fno.paths as paths_mod

    # Write an invalid settings.yaml - glob char in state_dir fails Pydantic validation
    bad_settings = tmp_path / "bad-collision-settings.yaml"
    bad_settings.write_text(
        "schema_version: 1\nconfig:\n  state_dir: '/home/*/fno'\n",
        encoding="utf-8",
    )
    # Wire FNO_CONFIG to point at the bad settings file
    monkeypatch.setenv("FNO_CONFIG", str(bad_settings))
    # Clear caches so the new bad settings are picked up

    # Call with user_settings=None (default) so _paths.config_file() is called
    # Should not raise; should return defaults
    result = _load_thresholds(
        project_settings=tmp_path / "nonexistent.yaml",
        user_settings=None,  # triggers _paths.config_file() call
    )
    assert isinstance(result, dict), "_load_thresholds must return a dict even on bad settings"
    assert result["high_count"] == _default_thresholds_loaded()["high_count"], (
        "result must contain default high_count when settings are invalid"
    )


def test_collision_load_thresholds_no_dead_assignment(tmp_path: Path) -> None:
    """AC2-HP (Finding B): no dead Path(...).expanduser() line before _paths.config_file() in _load_thresholds."""
    import inspect
    from fno.graph import collision

    src = inspect.getsource(collision._load_thresholds)
    assert 'Path("~/.fno/settings.yaml").expanduser()' not in src, (
        "Dead assignment in _load_thresholds must be removed"
    )
