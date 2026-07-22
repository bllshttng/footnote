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

    Writes a minimal settings.yaml so paths.X() resolves cleanly.
    Clears the paths._settings cache so the new settings take effect
    immediately (avoids the per-test monkeypatching issue described in
    feedback_default_arg_breaks_monkeypatch_isolation).

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

    # Clear both caches so the new env var takes effect immediately.
    # Must clear load_settings first, then _settings, so the next
    # _settings() call re-runs load_settings() with the new env.
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    paths_mod._settings.cache_clear()

    return settings
