"""Reapability classifier (x-5a30 task 1.1).

A worktree blocks removal only when it holds content removal would DESTROY.
A tracked file missing from disk is not that: its content is in the object
store at HEAD, so removing the worktree loses nothing.

Measured 2026-08-13: 17 of 20 dirty worktrees were dirty only because the same
76 tracked paths were missing, every one under a directory named `target`. The
old "is `git status --porcelain` empty" predicate blocked all 17, which is the
one class of dirt that cannot cause data loss.

The tests drive real temp git repos, not mocked porcelain strings, because the
classifier's job is to be right about what git actually prints.
"""
import subprocess
from pathlib import Path

import pytest

from fno.worktree_reapable import classify, reapable


def _git(cwd: Path, *args: str) -> str:
    r = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
    assert r.returncode == 0, f"git {' '.join(args)} failed: {r.stderr}"
    return r.stdout


@pytest.fixture()
def repo(tmp_path: Path) -> Path:
    wt = tmp_path / "wt"
    wt.mkdir()
    _git(wt, "init", "-q", "-b", "main")
    _git(wt, "config", "user.email", "t@example.com")
    _git(wt, "config", "user.name", "t")
    (wt / "keep.py").write_text("x = 1\n")
    (wt / "also.py").write_text("y = 2\n")
    (wt / ".gitignore").write_text("ignored/\n")
    _git(wt, "add", "-A")
    _git(wt, "commit", "-qm", "seed")
    return wt


# -- AC1-HP: deletions are recoverable ---------------------------------------


def test_deletions_only_is_reapable_and_counts_them(repo: Path) -> None:
    (repo / "keep.py").unlink()
    (repo / "also.py").unlink()

    v = reapable(repo)

    assert v.reapable is True
    assert v.reason == "clean"
    assert v.recoverable_deletions == 2


def test_staged_deletion_is_also_recoverable(repo: Path) -> None:
    _git(repo, "rm", "-q", "keep.py")

    v = reapable(repo)

    assert v.reapable is True
    assert v.recoverable_deletions == 1


def test_clean_worktree_is_reapable_with_zero_deletions(repo: Path) -> None:
    v = reapable(repo)

    assert v.reapable is True
    assert v.reason == "clean"
    assert v.recoverable_deletions == 0


# -- AC1-EDGE: modified tracked content blocks -------------------------------


def test_modified_tracked_file_blocks_and_names_it(repo: Path) -> None:
    (repo / "keep.py").write_text("x = 999\n")

    v = reapable(repo)

    assert v.reapable is False
    assert v.reason == "modified-tracked"
    assert "keep.py" in v.detail


def test_one_modification_beside_many_deletions_still_blocks(repo: Path) -> None:
    (repo / "also.py").unlink()
    (repo / "keep.py").write_text("x = 999\n")

    v = reapable(repo)

    assert v.reapable is False
    assert v.reason == "modified-tracked"


def test_staged_addition_blocks(repo: Path) -> None:
    (repo / "new.py").write_text("z = 3\n")
    _git(repo, "add", "new.py")

    v = reapable(repo)

    assert v.reapable is False
    assert v.reason == "modified-tracked"


# -- AC1-ERR: untracked non-ignored content blocks ---------------------------


def test_untracked_file_blocks(repo: Path) -> None:
    (repo / "scratch.py").write_text("nope\n")

    v = reapable(repo)

    assert v.reapable is False
    assert v.reason == "untracked"
    assert "scratch.py" in v.detail


def test_ignored_file_is_invisible_and_does_not_block(repo: Path) -> None:
    (repo / "ignored").mkdir()
    (repo / "ignored" / "junk.bin").write_text("junk\n")

    v = reapable(repo)

    assert v.reapable is True
    assert v.reason == "clean"


# -- Conflicts are never recoverable, even when both sides deleted -----------


@pytest.mark.parametrize("code", ["DD", "AU", "UD", "UA", "DU", "AA", "UU"])
def test_unmerged_codes_block_even_when_only_D_chars(code: str) -> None:
    """`DD` is "both deleted", a CONFLICT, not two recoverable deletions.

    Classifying it on its letters alone reads it as recoverable and throws
    away a merge the user has not resolved.
    """
    v = classify(f"{code} conflicted.py\n")

    assert v.reapable is False
    assert v.reason == "unmerged"


# -- Probe failure fails CLOSED ----------------------------------------------


def test_non_repo_path_fails_closed(tmp_path: Path) -> None:
    plain = tmp_path / "not-a-repo"
    plain.mkdir()

    v = reapable(plain)

    assert v.reapable is False
    assert v.reason == "probe-failed"


def test_missing_path_fails_closed(tmp_path: Path) -> None:
    v = reapable(tmp_path / "gone")

    assert v.reapable is False
    assert v.reason == "probe-failed"


# -- The receipt line the bash and rust callers parse ------------------------


def test_receipt_line_is_one_parseable_line(repo: Path) -> None:
    (repo / "keep.py").unlink()

    line = reapable(repo).line()

    assert line.startswith("reapable=yes ")
    assert "reason=clean" in line
    assert "recoverable_deletions=1" in line
    assert "\n" not in line


def test_blocking_receipt_names_the_reason_and_detail(repo: Path) -> None:
    (repo / "scratch.py").write_text("nope\n")

    line = reapable(repo).line()

    assert line.startswith("reapable=no ")
    assert "reason=untracked" in line
    assert "detail=scratch.py" in line


def test_detail_never_breaks_the_line_grammar(repo: Path) -> None:
    """A path with a space must not split the receipt into fake fields."""
    (repo / "two words.py").write_text("nope\n")

    line = reapable(repo).line()

    assert "\n" not in line
    assert line.count("reapable=") == 1


# -- Pure classify: the contract the equivalence test pins --------------------


def test_classify_is_pure_over_porcelain_text() -> None:
    text = " D a.py\nD  b.py\n D c.py\n"

    v = classify(text)

    assert v.reapable is True
    assert v.recoverable_deletions == 3


def test_classify_empty_is_clean() -> None:
    assert classify("").reapable is True
    assert classify("").recoverable_deletions == 0
