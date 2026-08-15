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

    ``load_settings``/``_settings`` are process-lifetime ``@cache``d. Clearing
    them in place (the old approach) took effect immediately but had no
    teardown: the cleared cache repopulates with THIS test's tmp_path and
    stays that way for the rest of the worker process, so any later test in
    the same xdist worker that resolves a path without its own isolation
    silently inherits this test's tmp_path - including any corrupt fixture
    file left on it. Swapping in fresh wrapper functions via ``monkeypatch``
    instead means the ORIGINAL cached functions are never touched, so
    ``monkeypatch``'s automatic teardown restores them exactly as they were
    before this test ran, with no leftover state to leak forward.

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

    # Fresh, empty caches for this test only - the module's real cached
    # functions are restored untouched when monkeypatch reverts at teardown.
    #
    # Reach caveat: this patch covers attribute access (``config.load_settings``,
    # ``paths._settings``) only. A module that bound ``load_settings`` at import
    # time (``from fno.config import load_settings`` at top level) keeps the
    # original cached function and does NOT see the tmp settings. No current
    # test exercises one of those modules through this fixture; if one lands,
    # combine this swap with a one-shot cache_clear of the originals here.
    import functools

    from fno import config as config_mod
    import fno.paths as paths_mod

    monkeypatch.setattr(  # type: ignore[attr-defined]
        config_mod,
        "load_settings",
        functools.lru_cache(maxsize=1)(config_mod.load_settings.__wrapped__),
    )
    monkeypatch.setattr(  # type: ignore[attr-defined]
        paths_mod,
        "_settings",
        functools.cache(paths_mod._settings.__wrapped__),
    )

    return settings
