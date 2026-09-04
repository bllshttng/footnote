"""The `fno do state` default path follows the manifest into the repo's space."""
from __future__ import annotations

from pathlib import Path

from fno.paths import target_state_path
from fno.state.cli import _resolve_path


def test_default_resolves_space_first_with_legacy_fallback(
    tmp_path: Path, monkeypatch
) -> None:
    repo = tmp_path / "repo"
    (repo / ".fno").mkdir(parents=True)
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo))
    monkeypatch.setenv("FNO_SPACES_DIR", str(tmp_path / "spaces"))

    # A manifest on the space answers from there; the checkout path is never read.
    space_manifest = target_state_path(repo)
    space_manifest.parent.mkdir(parents=True)
    space_manifest.write_text("---\nsession_id: s\n---\n", encoding="utf-8")
    assert _resolve_path(None, None) == space_manifest
    assert _resolve_path(None, "target") == space_manifest

    # A pre-space manifest still answers when the space copy is absent.
    space_manifest.unlink()
    legacy = repo / ".fno" / "target-state.md"
    legacy.write_text("---\nsession_id: x\n---\n", encoding="utf-8")
    assert _resolve_path(None, None) == legacy

    # Another state type keeps its checkout-relative spelling.
    assert _resolve_path(None, "session") == repo / ".fno" / "session-state.md"
