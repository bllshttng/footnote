"""FNO_NO_CANONICAL_CONFIG drops the canonical-config climb (preflight seam).

A linked worktree reaches the canonical checkout's ``.fno/config.toml`` via the
shared git-common-dir (candidate #3 of ``_settings_yaml_locations``). Inside the
hermetic preflight env that leak makes path/worktree tests assert on a
``worktrees_base`` a fresh CI checkout never has. This flag lets ``run_hermetic``
drop only that candidate; candidate #1 (FNO_CONFIG) and #2 (worktree-local) still
win, so real worktrees are unaffected (x-bbe7 US2/US3b).
"""
from __future__ import annotations

from pathlib import Path

import pytest


def _clear_caches() -> None:
    from fno import config as config_mod
    from fno import paths as paths_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    if hasattr(paths_mod, "_settings"):
        paths_mod._settings.cache_clear()  # type: ignore[attr-defined]


@pytest.fixture(autouse=True)
def _isolate(monkeypatch: pytest.MonkeyPatch):
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    monkeypatch.delenv("FNO_NO_CANONICAL_CONFIG", raising=False)
    _clear_caches()
    yield
    _clear_caches()


def _pin(monkeypatch: pytest.MonkeyPatch, *, worktree: Path, canonical: Path) -> None:
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "resolve_repo_root", lambda: worktree)
    monkeypatch.setattr(paths_mod, "resolve_canonical_repo_root", lambda: canonical)
    _clear_caches()


def _locs(monkeypatch, tmp_path) -> list[Path]:
    from fno.config import _settings_yaml_locations

    worktree = tmp_path / "wt"
    canonical = tmp_path / "canonical"
    _pin(monkeypatch, worktree=worktree, canonical=canonical)
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "global.yaml"))
    monkeypatch.setenv("FNO_CONFIG_SEARCH_ROOT", str(tmp_path))
    return _settings_yaml_locations()


def test_ac2_hp_flag_drops_canonical_candidate(tmp_path, monkeypatch):
    monkeypatch.setenv("FNO_NO_CANONICAL_CONFIG", "1")
    locs = _locs(monkeypatch, tmp_path)
    assert tmp_path / "wt" / ".fno" / "settings.yaml" in locs
    assert tmp_path / "canonical" / ".fno" / "settings.yaml" not in locs


def test_ac6_edge_default_keeps_canonical_candidate(tmp_path, monkeypatch):
    # Flag unset: real-worktree resolution unchanged, canonical still climbs.
    locs = _locs(monkeypatch, tmp_path)
    assert tmp_path / "canonical" / ".fno" / "settings.yaml" in locs


@pytest.mark.parametrize("value", ["", "0", "true", "yes", "2"])
def test_ac3_err_flag_inert_unless_exactly_one(tmp_path, monkeypatch, value):
    monkeypatch.setenv("FNO_NO_CANONICAL_CONFIG", value)
    locs = _locs(monkeypatch, tmp_path)
    assert tmp_path / "canonical" / ".fno" / "settings.yaml" in locs


def test_flag_never_drops_worktree_or_fno_config(tmp_path, monkeypatch):
    # Candidate #1 (FNO_CONFIG) still short-circuits even with the flag set.
    from fno.config import _settings_yaml_locations

    pinned = tmp_path / "pinned.yaml"
    monkeypatch.setenv("FNO_NO_CANONICAL_CONFIG", "1")
    monkeypatch.setenv("FNO_CONFIG", str(pinned))
    _clear_caches()
    assert _settings_yaml_locations() == [pinned]
