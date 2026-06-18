"""Tests for fno.paths.inbox_path (Wave 1.1).

Covers AC5-HP (resolves from settings, template vars expanded) and
AC5-EDGE (symlinked vault path resolves to the canonical target so
sibling worktrees share one file).

inbox_path() is the backlog *capture-tier* file resolver. It is distinct
from inbox_dir() (cross-project messaging) — see Domain Pitfalls in the
design doc.
"""
from __future__ import annotations

from pathlib import Path
from typing import Generator

import pytest


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    import fno.paths as paths_mod
    paths_mod._settings.cache_clear()
    paths_mod.resolve_repo_root.cache_clear()
    yield
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()
    paths_mod.resolve_repo_root.cache_clear()


def test_inbox_path_default_anchors_to_repo_root(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Obsidian on, no override, no legacy file -> canonical parking-lot.md."""
    from fno.paths_testing import use_tmpdir
    settings = use_tmpdir(monkeypatch, tmp_path)
    settings.write_text(
        "schema_version: 1\n"
        "config:\n"
        f"  state_dir: {tmp_path}/.fno/\n"
        "  obsidian:\n"
        "    enabled: true\n"
        f"    vault: {tmp_path}/vault\n",
        encoding="utf-8",
    )
    import fno.paths as paths_mod
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()

    result = paths_mod.inbox_path(project_root=tmp_path)
    assert result == (tmp_path / "internal/fno/backlog/parking-lot.md").resolve()


def test_inbox_path_default_no_vault_uses_state_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No Obsidian vault, no override, no legacy -> .fno/backlog/parking-lot.md.

    A non-vault repo must not get a stray internal/ directory materialized
    by the capture-inbox writer.
    """
    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)

    from fno.paths import inbox_path
    result = inbox_path(project_root=tmp_path)
    assert result == (tmp_path / ".fno/backlog/parking-lot.md").resolve()


def test_inbox_path_no_vault_prefers_existing_internal(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Back-compat: a pre-existing internal/ inbox is kept when no vault is set.

    A non-vault repo that already captured fu-* items to the old
    internal/fno/backlog/inbox.md keeps using it, so an upgrade never
    silently strands those items (codex review, PR #424).
    """
    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)
    legacy = tmp_path / "internal" / "fno" / "backlog" / "inbox.md"
    legacy.parent.mkdir(parents=True)
    legacy.write_text("# existing captures\n", encoding="utf-8")

    from fno.paths import inbox_path
    result = inbox_path(project_root=tmp_path)
    assert result == legacy.resolve()


def test_inbox_path_honors_settings_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC5-HP: config.paths.inbox_path is respected."""
    from fno.paths_testing import use_tmpdir
    settings = use_tmpdir(monkeypatch, tmp_path)
    custom = tmp_path / "custom" / "my-inbox.md"
    settings.write_text(
        "schema_version: 1\n"
        "config:\n"
        f"  state_dir: {tmp_path}/.fno/\n"
        "  paths:\n"
        f"    inbox_path: {custom}\n",
        encoding="utf-8",
    )
    import fno.paths as paths_mod
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()

    result = paths_mod.inbox_path(project_root=tmp_path)
    assert result == custom.resolve()


def test_inbox_path_expands_project_template_var(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC5-HP: {project} template var expands."""
    from fno.paths_testing import use_tmpdir
    settings = use_tmpdir(monkeypatch, tmp_path)
    settings.write_text(
        "schema_version: 1\n"
        "config:\n"
        f"  state_dir: {tmp_path}/.fno/\n"
        "  project:\n"
        "    id: myproj\n"
        "  paths:\n"
        f"    inbox_path: {tmp_path}/{{project}}/inbox.md\n",
        encoding="utf-8",
    )
    import fno.paths as paths_mod
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()

    result = paths_mod.inbox_path(project_root=tmp_path)
    assert result == (tmp_path / "myproj" / "inbox.md").resolve()


def test_inbox_path_resolves_through_symlink(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC5-EDGE: a symlinked `internal/` resolves to the canonical target.

    Two sibling worktrees each have their own `internal` symlink pointing
    at the same canonical vault dir. inbox_path() must resolve to that
    shared target so a lock on the target coordinates both. (Obsidian on:
    the internal/ default only applies when a vault is configured.)
    """
    from fno.paths_testing import use_tmpdir
    settings = use_tmpdir(monkeypatch, tmp_path)
    settings.write_text(
        "schema_version: 1\n"
        "config:\n"
        f"  state_dir: {tmp_path}/.fno/\n"
        "  obsidian:\n"
        "    enabled: true\n"
        f"    vault: {tmp_path}/vault\n",
        encoding="utf-8",
    )
    import fno.paths as paths_mod
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()

    canonical = tmp_path / "canonical_vault"
    canonical.mkdir()
    worktree = tmp_path / "wt"
    worktree.mkdir()
    (worktree / "internal").symlink_to(canonical)

    from fno.paths import inbox_path
    result = inbox_path(project_root=worktree)
    # .resolve() follows the symlink -> path lands under canonical, not wt/internal
    assert str(result).startswith(str(canonical.resolve()))
    assert result == (canonical / "fno/backlog/parking-lot.md").resolve()


def test_inbox_path_distinct_from_messaging_inbox_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """inbox_path (capture file) must not collide with inbox_dir (messaging)."""
    from fno.paths_testing import use_tmpdir
    use_tmpdir(monkeypatch, tmp_path)

    from fno.paths import inbox_dir, inbox_path
    assert inbox_path(project_root=tmp_path) != inbox_dir(project_root=tmp_path)


def test_inbox_path_honors_post_merge_parking_lot_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """config.post_merge.parking_lot_path (the producer's per-project queue) drives
    the capture-tier resolver, so add/list/tidy and /fno:pr merged all point
    at ONE file in every repo - not the fno-area default, which would make
    producer-written items invisible to the read commands (codex review, PR #434).
    """
    from fno.paths_testing import use_tmpdir
    settings = use_tmpdir(monkeypatch, tmp_path)
    settings.write_text(
        "schema_version: 1\n"
        "config:\n"
        f"  state_dir: {tmp_path}/.fno/\n"
        "  obsidian:\n"
        "    enabled: true\n"
        f"    vault: {tmp_path}/vault\n"
        "  post_merge:\n"
        "    parking_lot_path: internal/etl/backlog/parking-lot.md\n",
        encoding="utf-8",
    )
    import fno.paths as paths_mod
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()

    result = paths_mod.inbox_path(project_root=tmp_path)
    assert result == (tmp_path / "internal/etl/backlog/parking-lot.md").resolve()


def test_inbox_path_explicit_override_beats_post_merge(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """config.paths.inbox_path (explicit capture-tier override) wins over
    config.post_merge.parking_lot_path when both are set."""
    from fno.paths_testing import use_tmpdir
    settings = use_tmpdir(monkeypatch, tmp_path)
    custom = tmp_path / "custom" / "my-inbox.md"
    settings.write_text(
        "schema_version: 1\n"
        "config:\n"
        f"  state_dir: {tmp_path}/.fno/\n"
        "  paths:\n"
        f"    inbox_path: {custom}\n"
        "  post_merge:\n"
        "    parking_lot_path: internal/etl/backlog/parking-lot.md\n",
        encoding="utf-8",
    )
    import fno.paths as paths_mod
    from fno import config as config_mod
    config_mod.load_settings.cache_clear()  # type: ignore[attr-defined]
    paths_mod._settings.cache_clear()

    result = paths_mod.inbox_path(project_root=tmp_path)
    assert result == custom.resolve()
