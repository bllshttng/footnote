"""Test fixture helper for isolating path-config state.

Usage:
    def test_foo(tmp_path, monkeypatch):
        use_tmpdir(monkeypatch, tmp_path)
        # All paths.X() now resolve under tmp_path; no real state touched.

Import: from fno.paths_testing import use_tmpdir
"""
from __future__ import annotations

from pathlib import Path


def use_tmpdir(monkeypatch: object, tmp_path: Path) -> Path:
    """Point state_dir and settings file at tmp_path.

    Writes a minimal settings.yaml so paths.X() resolves cleanly, then sets
    ``FNO_CONFIG`` at the tmp file. The settings cache keys on its
    declaration (``fno.config._settings_key``), so every reader - including
    modules that bound ``load_settings`` at import time - resolves the tmp
    root with no function swap and no cache clearing.

    Returns the path to the tmp settings file for further customization
    (caller can overwrite it before calling paths.X()).
    """
    tmp_state = tmp_path / ".fno"
    tmp_state.mkdir(exist_ok=True)
    settings = tmp_state / "settings.yaml"
    settings.write_text(
        f"schema_version: 1\nconfig:\n  state_dir: {str(tmp_state)}/\n",
        encoding="utf-8",
    )
    sentinel = tmp_state / ".path-migration-done"
    sentinel.touch()

    # Wire the env var so load_settings() finds the tmp file
    monkeypatch.setenv("FNO_CONFIG", str(settings))  # type: ignore[attr-defined]

    return settings
