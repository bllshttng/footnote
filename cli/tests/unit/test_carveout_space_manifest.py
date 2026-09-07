"""A carve-out must find the session manifest where the manifest actually is.

`resolve_session_id` built `<repo>/.fno/target-state.md` by hand. A worktree
session's manifest does not live there: `fno do target init` writes it into the
project SPACE, which is what `fno.paths.target_state_path` resolves. So every
carve-out filed from a worktree recorded `session_id: null`, and an unscoped row
covers nothing at the plan-fidelity gate, which reads carve-outs by session.

Measured on 2026-09-07: a `/target` run in `.claude/worktrees/x-1f09` filed two
carve-outs against a live session and the gate still refused with
"0 covering carveout(s)".
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.carveout.core import resolve_session_id
from fno.paths_testing import use_tmpdir


def test_resolves_a_space_resident_manifest(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    from fno import paths

    repo_root = tmp_path / "checkout"
    (repo_root / ".git").mkdir(parents=True)

    manifest = paths.target_state_path(repo_root)
    manifest.parent.mkdir(parents=True, exist_ok=True)
    manifest.write_text(
        "---\nfno_id: 20260907T080957Z-cl70294-f60707\n---\n", encoding="utf-8"
    )
    # The hand-built path this used to read must NOT exist, or the test would
    # pass through the old code path and prove nothing.
    assert not (repo_root / ".fno" / "target-state.md").exists()

    assert resolve_session_id(repo_root) == "20260907T080957Z-cl70294-f60707"


def test_no_manifest_anywhere_still_resolves_none(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Capture is never lost over a missing session: None, not a raise."""
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.delenv("CLAUDECODE_SESSION_ID", raising=False)

    repo_root = tmp_path / "bare"
    (repo_root / ".git").mkdir(parents=True)
    assert resolve_session_id(repo_root) is None


def test_resolves_a_legacy_in_checkout_manifest(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A pre-space worktree still resolves. The space path exists as a concept
    but holds no file, so a resolver that only falls back on a RAISE reads None
    and files the carve-out unscoped again."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno import paths

    repo_root = tmp_path / "legacy"
    (repo_root / ".git").mkdir(parents=True)
    legacy = repo_root / ".fno" / "target-state.md"
    legacy.parent.mkdir(parents=True)
    legacy.write_text("---\nfno_id: legacy-run-1\n---\n", encoding="utf-8")
    assert not paths.target_state_path(repo_root).exists()

    assert resolve_session_id(repo_root) == "legacy-run-1"
