"""A linked worktree must read the canonical checkout's project-local settings.

Regression test for the gap where `fno config` resolved "project-local"
settings via `git rev-parse --show-toplevel` (the worktree, whose per-worktree
.fno/ may carry no settings.yaml) and so fell straight through to global
config, reporting project-local keys like config.post_merge.parking_lot_path empty.

The fix adds a canonical candidate (git --git-common-dir) between the
worktree-local and global candidates. These tests monkeypatch the two repo-root
resolvers to distinct dirs so the candidate walk is exercised without standing
up real git worktrees.
"""
from __future__ import annotations

from pathlib import Path

import pytest


def _clear_caches() -> None:
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]


@pytest.fixture(autouse=True)
def _isolate(monkeypatch: pytest.MonkeyPatch):
    """No explicit config; no FNO_REPO_ROOT pin; global candidate disabled."""
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    # The canonical candidate is the whole subject here, and preflight's
    # hermetic runner exports FNO_NO_CANONICAL_CONFIG=1 to drop it - so without
    # this the suite is red under preflight and green everywhere else.
    monkeypatch.delenv("FNO_NO_CANONICAL_CONFIG", raising=False)
    # /dev/null is not a regular file, so the per-user global candidate never
    # satisfies the load (documented test-isolation hook on _global_settings_path).
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")
    _clear_caches()
    yield
    _clear_caches()


def _write_settings(root: Path, parking_lot_path: str) -> None:
    fnodir = root / ".fno"
    fnodir.mkdir(parents=True, exist_ok=True)
    (fnodir / "settings.yaml").write_text(
        "schema_version: 1\nconfig:\n  post_merge:\n"
        f"    parking_lot_path: {parking_lot_path}\n",
        encoding="utf-8",
    )


def _pin_roots(
    monkeypatch: pytest.MonkeyPatch, *, worktree: Path, canonical: Path
) -> None:
    import fno.paths as paths_mod

    monkeypatch.setattr(paths_mod, "resolve_repo_root", lambda: worktree)
    monkeypatch.setattr(paths_mod, "resolve_canonical_repo_root", lambda: canonical)
    _clear_caches()


def test_worktree_reads_canonical_settings(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Worktree has NO settings.yaml; canonical has one -> load reads canonical."""
    canonical = tmp_path / "canonical"
    worktree = tmp_path / "worktree"
    worktree.mkdir(parents=True)
    _write_settings(canonical, "internal/etl/backlog/parking-lot.md")

    _pin_roots(monkeypatch, worktree=worktree, canonical=canonical)

    from fno.config import load_settings

    settings = load_settings()
    assert settings.post_merge.parking_lot_path == "internal/etl/backlog/parking-lot.md"


def test_worktree_local_settings_win_over_canonical(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A real worktree-local settings.yaml still wins over canonical (override)."""
    canonical = tmp_path / "canonical"
    worktree = tmp_path / "worktree"
    _write_settings(canonical, "internal/canonical/parking-lot.md")
    _write_settings(worktree, "internal/worktree/parking-lot.md")

    _pin_roots(monkeypatch, worktree=worktree, canonical=canonical)

    from fno.config import load_settings

    settings = load_settings()
    assert settings.post_merge.parking_lot_path == "internal/worktree/parking-lot.md"


def test_candidate_paths_include_canonical_in_worktree(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """In a worktree the candidate order is [worktree, canonical, global]."""
    canonical = tmp_path / "canonical"
    worktree = tmp_path / "worktree"

    _pin_roots(monkeypatch, worktree=worktree, canonical=canonical)

    from fno.config import _candidate_paths

    cands = _candidate_paths()
    # config.toml is preferred over settings.yaml at each location, so each dir
    # contributes its config.toml first, then its settings.yaml.
    assert cands[0] == worktree / ".fno" / "config.toml"
    assert cands[1] == worktree / ".fno" / "settings.yaml"
    assert cands[2] == canonical / ".fno" / "config.toml"
    assert cands[3] == canonical / ".fno" / "settings.yaml"


def test_candidate_paths_dedup_when_canonical_equals_worktree(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """From the canonical checkout (root == canonical) there is no duplicate."""
    root = tmp_path / "repo"

    _pin_roots(monkeypatch, worktree=root, canonical=root)

    from fno.config import _candidate_paths

    cands = _candidate_paths()
    project_local = [
        c for c in cands if c.name == "settings.yaml" and ".fno" in c.parts
    ]
    assert project_local == [root / ".fno" / "settings.yaml"]
