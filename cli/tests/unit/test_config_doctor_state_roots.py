"""Tests for the root receipt in `fno config doctor` (x-3d21, leg 2).

`fno config get review.max_rounds` answered from the canonical root while the
gate resolver answered from the worktree. Both returned a value the whole
time, so asserting that the receipt returns something proves nothing. Every
test here asserts the ROOT it names, and the divergence control asserts a
positive marker - the WARNING line with both roots in it - rather than an
absence.
"""
from __future__ import annotations

from pathlib import Path

import pytest
import typer

from fno.config_cli import _report_state_roots


def _capture(monkeypatch: pytest.MonkeyPatch) -> list[str]:
    out: list[str] = []
    monkeypatch.setattr(typer, "echo", lambda m="", **k: out.append(str(m)))
    return out


def _pin_roots(monkeypatch: pytest.MonkeyPatch, worktree: Path, canonical: Path) -> None:
    import fno.paths as paths_mod

    worktree.mkdir(parents=True, exist_ok=True)
    canonical.mkdir(parents=True, exist_ok=True)
    monkeypatch.setattr(paths_mod, "resolve_repo_root", lambda: worktree)
    monkeypatch.setattr(paths_mod, "resolve_canonical_repo_root", lambda: canonical)


def test_receipt_names_every_state_class_with_its_root_class_and_selector(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    from fno.paths import STATE_FILES

    _pin_roots(monkeypatch, tmp_path / "repo", tmp_path / "repo")
    out = _capture(monkeypatch)

    _report_state_roots()

    text = "\n".join(out)
    for row in STATE_FILES:
        assert row.filename in text
        assert row.root_class in text


def test_receipt_names_the_resolved_root_not_merely_a_value(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """The whole point of leg 2: print WHICH root, not that a root exists."""
    _pin_roots(monkeypatch, tmp_path / "repo", tmp_path / "repo")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claimhome"))
    out = _capture(monkeypatch)

    _report_state_roots()

    text = "\n".join(out)
    assert str(tmp_path / "claimhome" / ".fno" / "claims") in text


def test_receipt_warns_when_the_worktree_and_canonical_roots_differ(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """The positive control. The marker is the WARNING line naming BOTH roots.

    This is the line that would have caught x-79a6 the first time anyone
    looked at it: a linked worktree reads project config from canonical while
    its own state resolvers stay on the worktree root, and nothing said so.
    """
    worktree = tmp_path / "repo" / ".claude" / "worktrees" / "feat"
    canonical = tmp_path / "repo"
    _pin_roots(monkeypatch, worktree, canonical)
    out = _capture(monkeypatch)

    _report_state_roots()

    text = "\n".join(out)
    assert "WARNING" in text
    assert str(worktree) in text
    assert str(canonical) in text


def test_receipt_stays_silent_on_the_canonical_checkout(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """The negative control: on canonical the two roots agree, so no warning."""
    canonical = tmp_path / "repo"
    _pin_roots(monkeypatch, canonical, canonical)
    out = _capture(monkeypatch)

    _report_state_roots()

    assert "WARNING" not in "\n".join(out)


def test_receipt_names_the_state_file_that_has_no_resolver(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """`target-state.md` is built by hand at four sites, and the receipt says so
    rather than printing a path that implies an owner exists."""
    _pin_roots(monkeypatch, tmp_path / "repo", tmp_path / "repo")
    out = _capture(monkeypatch)

    _report_state_roots()

    line = next(ln for ln in out if "target-state.md" in ln)
    assert "NO RESOLVER" in line
