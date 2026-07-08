"""FNO_CONFIG_SEARCH_ROOT bounds the config candidate chain to the tmpdir.

The config candidate chain climbs to the canonical checkout via
``git worktree list``. During a local full-suite run that reached the
developer's real ~/code/footnote/footnote/.fno/config.toml (auto_install=false,
YAML-misparse warning) - green in CI, red locally. The ceiling drops any
candidate resolving outside the allowed test roots so a poisoned parent-dir
config can never leak in.
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
    _clear_caches()
    yield
    _clear_caches()


def _pin(monkeypatch: pytest.MonkeyPatch, *, worktree: Path, canonical: Path) -> None:
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "resolve_repo_root", lambda: worktree)
    monkeypatch.setattr(paths_mod, "resolve_canonical_repo_root", lambda: canonical)
    _clear_caches()


def test_canonical_candidate_outside_ceiling_is_dropped(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A canonical .fno outside the ceiling never becomes a read candidate."""
    from fno.config import _settings_yaml_locations

    worktree = tmp_path / "wt"
    poison_checkout = tmp_path / "canonical"  # stands in for ~/code/.../footnote
    _pin(monkeypatch, worktree=worktree, canonical=poison_checkout)
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "global.yaml"))
    monkeypatch.setenv("FNO_CONFIG_SEARCH_ROOT", str(worktree))

    locs = _settings_yaml_locations()

    assert worktree / ".fno" / "settings.yaml" in locs
    assert poison_checkout / ".fno" / "settings.yaml" not in locs


def test_poisoned_parent_config_not_loaded(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """load_settings() ignores a poisoned config.toml outside the ceiling.

    Mirrors the triage-health failure: a real config with a YAML-misparse-prone
    body sits at the canonical root; with the ceiling pinned to the worktree it
    is never read, so loaded_from() stays inside the ceiling (or None).
    """
    from fno.config import load_settings, loaded_from

    worktree = tmp_path / "wt"
    poison_checkout = tmp_path / "canonical"
    poison = poison_checkout / ".fno" / "config.toml"
    poison.parent.mkdir(parents=True)
    # A value the ceiling must NOT let leak into the loaded model.
    poison.write_text('[project]\nid = "LEAKED_FROM_CANONICAL"\n', encoding="utf-8")

    _pin(monkeypatch, worktree=worktree, canonical=poison_checkout)
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "global.yaml"))
    monkeypatch.setenv("FNO_CONFIG_SEARCH_ROOT", str(worktree))

    settings = load_settings()

    assert settings.project.id != "LEAKED_FROM_CANONICAL"
    lf = loaded_from()
    assert lf is None or lf.resolve().is_relative_to(worktree.resolve())


def test_direct_reader_candidates_are_bounded(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """config_read_candidates (the _intake project<->path reader path) is bounded.

    The actual leak: a cwd-relative .fno/config.toml resolves through a
    worktree symlink to the canonical checkout, and _intake yaml-parses it,
    polluting --json stdout. That reader funnels through config_read_candidates,
    NOT _settings_yaml_locations, so the ceiling must bound this path too.
    """
    from fno.config import config_read_candidates

    inside = tmp_path / "wt" / ".fno" / "settings.yaml"
    outside = tmp_path / "canonical" / ".fno" / "settings.yaml"
    monkeypatch.setenv("FNO_CONFIG_SEARCH_ROOT", str(tmp_path / "wt"))

    cands = config_read_candidates([inside, outside])

    # config.toml-first sibling of the inside candidate survives...
    assert inside.with_name("config.toml") in cands
    # ...but nothing under the out-of-ceiling canonical root leaks in.
    assert all("canonical" not in str(c) for c in cands)


def test_no_ceiling_env_is_a_noop(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Unset FNO_CONFIG_SEARCH_ROOT leaves the full chain intact (production)."""
    from fno.config import _settings_yaml_locations

    worktree = tmp_path / "wt"
    canonical = tmp_path / "canonical"
    _pin(monkeypatch, worktree=worktree, canonical=canonical)
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(tmp_path / "global.yaml"))
    monkeypatch.delenv("FNO_CONFIG_SEARCH_ROOT", raising=False)

    locs = _settings_yaml_locations()

    assert canonical / ".fno" / "settings.yaml" in locs
