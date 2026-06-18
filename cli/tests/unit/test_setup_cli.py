"""Tests for fno.setup_cli (fno setup migrate-paths).

Finding E (P2): migrate_paths_cmd() calls run_migration(force=force) with no
settings_root arg, so the CLI surface always writes to ~/.fno regardless
of config.state_dir. Both the CLI and _check_migration must agree on the
active state_dir.
"""
from __future__ import annotations

from pathlib import Path
from typing import Generator
from unittest.mock import patch

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
# AC-E-HP: fno setup migrate-paths honors active state_dir
# ---------------------------------------------------------------------------


def test_migrate_paths_cmd_passes_state_dir_to_run_migration(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC-E-HP: migrate_paths_cmd() passes settings_root=_paths.state_dir() to run_migration.

    Finding E: without this, `fno setup migrate-paths` ignores config.state_dir
    and always writes to ~/.fno.

    Strategy: configure a custom state_dir, invoke migrate_paths_cmd via
    typer CliRunner, intercept run_migration, verify settings_root kwarg.
    """
    custom_state = tmp_path / "custom-abi-e"
    custom_state.mkdir()

    # Write settings pointing to custom state_dir
    abilities_dir = tmp_path / ".fno"
    abilities_dir.mkdir()
    settings_file = abilities_dir / "settings.yaml"
    settings_file.write_text(
        f"schema_version: 1\nconfig:\n  state_dir: '{custom_state}'\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_CONFIG", str(settings_file))

    # Clear caches so new config is picked up
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

    captured_calls: list[dict] = []

    def _mock_run_migration(**kwargs: object) -> int:
        captured_calls.append(dict(kwargs))
        return 0

    from typer.testing import CliRunner
    from fno.setup_cli import app

    with patch("fno.setup.migrate_paths.run_migration", side_effect=_mock_run_migration):
        runner = CliRunner(env={"COLUMNS": "240", "NO_COLOR": "1"})
        # 'fno setup' now has multiple subcommands (migrate-paths, post-merge),
        # so the command under test must be named explicitly.
        result = runner.invoke(app, ["migrate-paths"])

    assert result.exit_code == 0, f"Expected exit 0; got {result.exit_code}\n{result.output}"
    assert captured_calls, "migrate_paths_cmd must call run_migration"
    call_kwargs = captured_calls[0]

    assert "settings_root" in call_kwargs, (
        f"run_migration must be called with settings_root kwarg; got: {call_kwargs}"
    )
    assert call_kwargs["settings_root"] == custom_state, (
        f"settings_root must equal _paths.state_dir() ({custom_state!s}); "
        f"got: {call_kwargs['settings_root']}"
    )


# ---------------------------------------------------------------------------
# Finding E (P2, round 5+6): migration print_summary writes to stderr, not stdout
# ---------------------------------------------------------------------------


def test_print_summary_writes_to_stderr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture
) -> None:
    """Finding E (P2): print_summary() output goes to stderr, not stdout.

    `fno paths shell-stub` is consumed via $(fno paths shell-stub) in bash.
    If migration runs on first invocation and print_summary writes to stdout,
    the captured output contains the migration summary lines alongside the
    shell-stub path, breaking `source $(...)`.

    Fix: print_summary must use print(..., file=sys.stderr) for all lines.
    """
    from fno.setup.migrate_paths import print_summary

    detected: dict = {}
    settings_path = tmp_path / "settings.yaml"
    settings_path.write_text("schema_version: 1\n", encoding="utf-8")

    print_summary(detected, settings_path)

    captured = capsys.readouterr()
    # All migration summary output must go to stderr, nothing to stdout
    assert captured.out == "", (
        f"print_summary must write to stderr only; got stdout:\n{captured.out!r}"
    )
    assert "[setup]" in captured.err or "migration" in captured.err.lower(), (
        f"print_summary must write summary to stderr; got stderr:\n{captured.err!r}"
    )
