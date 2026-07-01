"""Per-worktree config override via `.fno/settings.local.yaml` (x-cbce).

setup-worktree.sh symlinks `.fno/settings.yaml` from canonical into every
worktree, which shares ALL config - including the collision-prone keys
(config.post_merge.parking_lot_path, config.project.id). The local override is
the one file kept per-worktree: it layers ONLY WORKTREE_LOCAL_KEYS on top of
the shared settings, ignoring anything else so a local file can never silently
fork shared config.

Tests anchor via FNO_CONFIG (a real settings.yaml in a tmp .fno/); the loader
looks for settings.local.yaml as its sibling.
"""
from __future__ import annotations

import logging
import os
from pathlib import Path


def _fno_dir(tmp_path: Path) -> Path:
    d = tmp_path / ".fno"
    d.mkdir(parents=True, exist_ok=True)
    return d


def _load(tmp_path, monkeypatch, shared: str, local: str | None):
    """Write shared settings.yaml (+ optional local), point FNO_CONFIG, load."""
    d = _fno_dir(tmp_path)
    (d / "settings.yaml").write_text(shared, encoding="utf-8")
    if local is not None:
        (d / "settings.local.yaml").write_text(local, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(d / "settings.yaml"))
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", os.devnull)
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    return config_mod.load_settings()


SHARED = (
    "schema_version: 1\n"
    "config:\n"
    "  post_merge:\n"
    "    parking_lot_path: shared/parking-lot.md\n"
    "    enabled: true\n"
    "  project:\n"
    "    id: shared-project\n"
)


def test_local_override_wins_for_allowlisted_keys(tmp_path, monkeypatch):
    local = (
        "config:\n"
        "  post_merge:\n"
        "    parking_lot_path: mine/parking-lot.md\n"
        "  project:\n"
        "    id: my-worktree\n"
    )
    s = _load(tmp_path, monkeypatch, SHARED, local)
    assert s.config.post_merge.parking_lot_path == "mine/parking-lot.md"
    assert s.config.project.id == "my-worktree"
    # A shared key not in the local file is untouched.
    assert s.config.post_merge.enabled is True


def test_absent_local_file_is_noop(tmp_path, monkeypatch):
    s = _load(tmp_path, monkeypatch, SHARED, None)
    assert s.config.post_merge.parking_lot_path == "shared/parking-lot.md"
    assert s.config.project.id == "shared-project"


def test_non_allowlisted_key_ignored_with_one_warning(tmp_path, monkeypatch, caplog):
    # Local file mixes an allowlisted key with a non-allowlisted one. Only the
    # allowlisted key applies; the other is dropped with a single warning.
    local = (
        "config:\n"
        "  post_merge:\n"
        "    parking_lot_path: mine/parking-lot.md\n"
        "    enabled: false\n"  # NOT worktree-local -> ignored
    )
    with caplog.at_level(logging.WARNING, logger="fno.config"):
        s = _load(tmp_path, monkeypatch, SHARED, local)
    assert s.config.post_merge.parking_lot_path == "mine/parking-lot.md"
    # Non-allowlisted key kept its shared value.
    assert s.config.post_merge.enabled is True
    warnings = [r for r in caplog.records if "settings.local.yaml" in r.getMessage()]
    assert len(warnings) == 1, [r.getMessage() for r in caplog.records]
    assert "config.post_merge.enabled" in warnings[0].getMessage()


def test_symlinked_local_file_is_skipped(tmp_path, monkeypatch):
    # A symlinked local file would re-share the collision-prone keys, defeating
    # the point -> skipped, shared value wins.
    d = _fno_dir(tmp_path)
    real = tmp_path / "elsewhere.yaml"
    real.write_text(
        "config:\n  post_merge:\n    parking_lot_path: symlinked/parking-lot.md\n",
        encoding="utf-8",
    )
    (d / "settings.local.yaml").symlink_to(real)
    (d / "settings.yaml").write_text(SHARED, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(d / "settings.yaml"))
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", os.devnull)
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    s = config_mod.load_settings()
    assert s.config.post_merge.parking_lot_path == "shared/parking-lot.md"


def test_worktree_local_override_filters_pure():
    # Unit-test the pure filter directly: allowlisted leaves kept, others dropped.
    from fno.config import _worktree_local_override

    out = _worktree_local_override(
        {
            "config": {
                "post_merge": {"parking_lot_path": "x", "enabled": False},
                "project": {"id": "y", "vision": "nope"},
            }
        }
    )
    assert out == {
        "config": {
            "post_merge": {"parking_lot_path": "x"},
            "project": {"id": "y"},
        }
    }


def test_production_anchor_via_repo_root(tmp_path, monkeypatch):
    # Production path (no FNO_CONFIG): the local file sits in <repo_root>/.fno/,
    # the same dir as the worktree's settings.yaml candidate.
    d = _fno_dir(tmp_path)
    (d / "settings.yaml").write_text(SHARED, encoding="utf-8")
    (d / "settings.local.yaml").write_text(
        "config:\n  project:\n    id: from-repo-root\n", encoding="utf-8"
    )
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", os.devnull)
    from fno import config as config_mod
    from fno import paths as paths_mod

    paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    try:
        s = config_mod.load_settings()
        assert s.config.project.id == "from-repo-root"
    finally:
        paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]


def test_allowlist_is_exactly_the_two_collision_keys():
    from fno.config import WORKTREE_LOCAL_KEYS

    assert WORKTREE_LOCAL_KEYS == frozenset(
        {"config.post_merge.parking_lot_path", "config.project.id"}
    )
