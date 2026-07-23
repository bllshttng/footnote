"""Per-worktree config override via `.fno/config.local.toml` (x-cbce; x-8526).

setup-worktree.sh symlinks `.fno/config.toml` from canonical into every
worktree, which shares ALL config. The local override is the one file kept
per-worktree: it layers ONLY WORKTREE_LOCAL_KEYS on top of the shared config,
ignoring anything else so a local file can never silently fork shared config.
x-071c narrowed the allowlist to the single key `project.id`;
`post_merge.parking_lot_path` was removed (the post-merge ritual now anchors on
the canonical root instead), so a lane-local parking_lot_path is ignored.

Post stage-3 the on-disk files are flat TOML (config.toml / config.local.toml),
so keys carry no `config.` prefix. Tests anchor via FNO_CONFIG (a real
config.toml in a tmp .fno/); the loader looks for config.local.toml as its
sibling.
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
    """Write shared config.toml (+ optional local), point FNO_CONFIG, load."""
    d = _fno_dir(tmp_path)
    (d / "config.toml").write_text(shared, encoding="utf-8")
    if local is not None:
        (d / "config.local.toml").write_text(local, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(d / "config.toml"))
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", os.devnull)
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    return config_mod.load_settings()


SHARED = (
    "schema_version = 1\n"
    "[post_merge]\n"
    'parking_lot_path = "shared/parking-lot.md"\n'
    "enabled = true\n"
    "[project]\n"
    'id = "shared-project"\n'
)


def test_project_id_override_wins_parking_lot_ignored(tmp_path, monkeypatch, caplog):
    """AC6-EDGE: project.id is the sole per-worktree key. A parking_lot_path
    override in the local file is ignored (x-071c dropped it from the allowlist)
    so the canonical value wins; project.id from the same file still overrides."""
    local = (
        "[post_merge]\n"
        'parking_lot_path = "mine/parking-lot.md"\n'  # no longer allowlisted -> ignored
        "[project]\n"
        'id = "my-worktree"\n'
    )
    with caplog.at_level(logging.WARNING, logger="fno.config"):
        s = _load(tmp_path, monkeypatch, SHARED, local)
    assert s.project.id == "my-worktree"  # allowlisted -> wins
    assert s.post_merge.parking_lot_path == "shared/parking-lot.md"  # ignored -> canonical
    # A shared key not in the local file is untouched.
    assert s.post_merge.enabled is True
    warnings = [r for r in caplog.records if "config.local.toml" in r.getMessage()]
    assert len(warnings) == 1, [r.getMessage() for r in caplog.records]
    assert "post_merge.parking_lot_path" in warnings[0].getMessage()


def test_absent_local_file_is_noop(tmp_path, monkeypatch):
    s = _load(tmp_path, monkeypatch, SHARED, None)
    assert s.post_merge.parking_lot_path == "shared/parking-lot.md"
    assert s.project.id == "shared-project"


def test_uncached_repo_loader_applies_worktree_local_override(tmp_path, monkeypatch):
    """Explicit repo loads must preserve the lane-local project identity."""
    d = _fno_dir(tmp_path)
    (d / "config.toml").write_text(SHARED, encoding="utf-8")
    (d / "config.local.toml").write_text(
        '[project]\nid = "lane-project"\n', encoding="utf-8"
    )
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", os.devnull)

    from fno.config import load_settings_for_repo

    settings = load_settings_for_repo(tmp_path)
    assert settings.project.id == "lane-project"
    assert settings.post_merge.parking_lot_path == "shared/parking-lot.md"


def test_non_allowlisted_key_ignored_with_one_warning(tmp_path, monkeypatch, caplog):
    # Local file mixes the allowlisted key (project.id) with a non-allowlisted
    # one. Only the allowlisted key applies; the other is dropped with one warning.
    local = (
        "[project]\n"
        'id = "my-worktree"\n'
        "[post_merge]\n"
        "enabled = false\n"  # NOT worktree-local -> ignored
    )
    with caplog.at_level(logging.WARNING, logger="fno.config"):
        s = _load(tmp_path, monkeypatch, SHARED, local)
    assert s.project.id == "my-worktree"
    # Non-allowlisted key kept its shared value.
    assert s.post_merge.enabled is True
    warnings = [r for r in caplog.records if "config.local.toml" in r.getMessage()]
    assert len(warnings) == 1, [r.getMessage() for r in caplog.records]
    assert "post_merge.enabled" in warnings[0].getMessage()


def test_symlinked_local_file_is_skipped(tmp_path, monkeypatch):
    # A symlinked local file would re-share the collision-prone key, defeating
    # the point -> skipped, shared value wins.
    d = _fno_dir(tmp_path)
    real = tmp_path / "elsewhere.toml"
    real.write_text(
        '[project]\nid = "symlinked-id"\n',
        encoding="utf-8",
    )
    (d / "config.local.toml").symlink_to(real)
    (d / "config.toml").write_text(SHARED, encoding="utf-8")
    monkeypatch.setenv("FNO_CONFIG", str(d / "config.toml"))
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", os.devnull)
    from fno import config as config_mod

    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    s = config_mod.load_settings()
    assert s.project.id == "shared-project"


def test_worktree_local_override_filters_pure():
    # Unit-test the pure filter directly: allowlisted leaves kept, others dropped.
    # Input is a flat dict (config.local.toml has no `config.` wrapper).
    from fno.config import _worktree_local_override

    out = _worktree_local_override(
        {
            "post_merge": {"parking_lot_path": "x", "enabled": False},
            "project": {"id": "y", "vision": "nope"},
        }
    )
    # x-071c: parking_lot_path is no longer allowlisted -> dropped with the rest.
    assert out == {"project": {"id": "y"}}


def test_production_anchor_via_repo_root(tmp_path, monkeypatch):
    # Production path (no FNO_CONFIG): a legacy settings.yaml + settings.local.yaml
    # sit in <repo_root>/.fno/. The loader auto-migrates BOTH to flat config.toml /
    # config.local.toml on load, then applies the worktree-local override.
    d = _fno_dir(tmp_path)
    (d / "settings.yaml").write_text(
        "schema_version: 1\n"
        "config:\n"
        "  post_merge:\n"
        "    parking_lot_path: shared/parking-lot.md\n"
        "  project:\n"
        "    id: shared-project\n",
        encoding="utf-8",
    )
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
        assert s.project.id == "from-repo-root"
        # Both files were migrated to flat TOML (hard cut).
        assert (d / "config.toml").is_file()
        assert (d / "config.local.toml").is_file()
        assert not (d / "settings.yaml").exists()
    finally:
        paths_mod.resolve_repo_root.cache_clear()  # type: ignore[attr-defined]


def test_non_string_key_in_local_does_not_crash(caplog):
    # The pure filter str-coerces non-string keys rather than TypeError on
    # sorted()/join() (Gemini review, PR #128). TOML keys are always strings, so
    # this is defensive; exercise it at the unit level with int/float keys.
    from fno.config import _worktree_local_override

    with caplog.at_level(logging.WARNING, logger="fno.config"):
        out = _worktree_local_override(
            {1: "bare-int", 3.14: "bare-float", "project": {"id": "still-works"}}
        )
    assert out == {"project": {"id": "still-works"}}
    warnings = [r for r in caplog.records if "config.local.toml" in r.getMessage()]
    assert len(warnings) == 1
    assert "1" in warnings[0].getMessage() and "3.14" in warnings[0].getMessage()


def test_allowlist_is_exactly_project_id():
    # x-071c narrowed the allowlist to the single collision key. post_merge.
    # parking_lot_path was removed - the ritual anchors on the canonical root.
    from fno.config import WORKTREE_LOCAL_KEYS

    assert WORKTREE_LOCAL_KEYS == frozenset({"project.id"})
