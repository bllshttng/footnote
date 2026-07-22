"""Tests for Codex PR #264 round 3 fixes (findings A, B, C, D).

Finding A: paths.sh self-sets REPO_ROOT so sourcing under set -u doesn't crash.
Finding B: dead-line regression in health_monitor.py and collision.py; fail-open.
Finding C: shell-stub regenerates from current settings, not static snapshot.
Finding D: plain-relative predicate in paths.py rejects env vars anywhere.
"""
from __future__ import annotations

import subprocess
import tempfile
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
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
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


def _set_settings(monkeypatch: pytest.MonkeyPatch, tmp_path: Path, content: str) -> None:
    settings_file = tmp_path / "settings.yaml"
    settings_file.write_text(content, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))


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
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

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
    from fno.graph.collision import _load_thresholds, DEFAULT_THRESHOLDS
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
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

    # Call with user_settings=None (default) so _paths.config_file() is called
    # Should not raise; should return defaults
    result = _load_thresholds(
        project_settings=tmp_path / "nonexistent.yaml",
        user_settings=None,  # triggers _paths.config_file() call
    )
    assert isinstance(result, dict), "_load_thresholds must return a dict even on bad settings"
    assert result["high_count"] == DEFAULT_THRESHOLDS["high_count"], (
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


# ===========================================================================
# Finding C: shell-stub regenerates from current settings
# ===========================================================================


def test_shell_stub_regenerates_per_call(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC3-HP (Finding C): shell_stub() generates a fresh stub and returns its path.

    Two calls with DIFFERENT settings must produce files with different content
    (the settings change is reflected in the generated stub).
    """
    # First call: default settings
    _set_settings(monkeypatch, tmp_path, "schema_version: 1\n")

    from fno.paths_cli import shell_stub as _  # noqa: just ensure importable
    from typer.testing import CliRunner
    from fno.cli import app

    runner = CliRunner()

    result1 = runner.invoke(
        app,
        ["paths", "shell-stub"],
        env={"FNO_REPO_ROOT": str(tmp_path), "COLUMNS": "240", "NO_COLOR": "1"},
        catch_exceptions=False,
    )
    assert result1.exit_code == 0, f"shell-stub failed: {result1.output}"
    path1 = result1.output.strip()
    assert path1, "shell-stub must print a path"

    # Change settings - custom plans_dir - then call again
    from fno import config as config_mod
    import fno.paths as paths_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\nconfig:\n  plans_dir: '.fno/my-custom-plans'\n",
    )
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

    result2 = runner.invoke(
        app,
        ["paths", "shell-stub"],
        env={"FNO_REPO_ROOT": str(tmp_path), "COLUMNS": "240", "NO_COLOR": "1"},
        catch_exceptions=False,
    )
    assert result2.exit_code == 0, f"shell-stub failed second call: {result2.output}"
    path2 = result2.output.strip()
    assert path2, "shell-stub must print a path on second call"

    # The path returned must be a readable file
    assert Path(path1).exists(), f"shell-stub path1 must be a readable file: {path1}"
    assert Path(path2).exists(), f"shell-stub path2 must be a readable file: {path2}"

    # Content must differ because settings changed
    content1 = Path(path1).read_text(encoding="utf-8")
    content2 = Path(path2).read_text(encoding="utf-8")
    assert content1 != content2, (
        "shell-stub must regenerate from current settings. "
        "Two calls with different settings must produce different files.\n"
        f"path1 content:\n{content1}\npath2 content:\n{content2}"
    )
    assert "my-custom-plans" in content2, (
        f"Second stub must reflect custom plans_dir, got:\n{content2}"
    )


# ===========================================================================
# Finding D: plain-relative predicate in paths.py rejects env vars anywhere
# ===========================================================================


def test_plans_dir_with_env_var_in_middle_expands(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC4-HP (Finding D): plans_dir with env var anywhere is expanded, not treated as plain-relative.

    A path like '.fno/$USER/plans' has a $ in the middle, not at the start.
    The plain-relative predicate must reject it (return False) so _resolve() is called
    and os.path.expandvars() runs.
    """
    import os
    monkeypatch.setenv("MYTEST_VAR", "testuser123")
    _set_settings(
        monkeypatch,
        tmp_path,
        "schema_version: 1\nconfig:\n  plans_dir: '.fno/$MYTEST_VAR/plans'\n",
    )

    from fno import config as config_mod
    import fno.paths as paths_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]

    from fno.paths import plans_dir

    result = plans_dir(project_root=tmp_path)
    # The $MYTEST_VAR should have been expanded
    assert "testuser123" in str(result), (
        f"plans_dir must expand env var anywhere in path. "
        f"MYTEST_VAR=testuser123 but got: {result}"
    )
    assert "$MYTEST_VAR" not in str(result), (
        f"plans_dir must not leave unexpanded $MYTEST_VAR in result: {result}"
    )


def test_is_plain_relative_rejects_dollar_anywhere() -> None:
    """AC4-HP (Finding D): plans_dir plain-relative predicate rejects '$' anywhere in path.

    The is_plain_relative check in paths.py:plans_dir must treat a path with
    '$' anywhere (not just at start) as non-plain-relative.
    """
    # We can't directly test the internal logic easily, so test via the public
    # plans_dir() function which uses it. But let's also verify the source.
    import inspect
    from fno import paths

    src = inspect.getsource(paths.plans_dir)
    # The fix adds: or "$" in raw
    # Check the source has the extended check
    assert '"$" in raw' in src or "'$' in raw" in src or "$ in" in src, (
        "plans_dir must check for '$' anywhere in the raw path string, not just at the start. "
        "Expected '\"$\" in raw' or similar in source."
    )
