"""Unit tests for the update modal's PR-grouped release notes (fno.update).

Every fixture is a REAL git repo with GitHub-style merge commits, so the rev
range the notes are built from is a real first-parent merge chain, not a
canned string.
"""

from __future__ import annotations

import os as _os
import subprocess as _sp
from pathlib import Path

from fno import update


def _run_git(directory: Path, *args: str) -> str:
    env = {
        **_os.environ,
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@e",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@e",
    }
    proc = _sp.run(
        ["git", *args], cwd=directory, check=True, capture_output=True, text=True, env=env
    )
    return proc.stdout.strip()


def _gh_merge_repo(directory: Path, prs: list[tuple[int, str]]) -> tuple[str, str]:
    """A repo whose main is a chain of GitHub-style merge commits (subject
    ``Merge pull request #N from ...``, body first line the PR title). Returns
    (base, head) so tests pass base as installed_rev."""
    directory.mkdir(parents=True, exist_ok=True)
    _run_git(directory, "init", "-q", "-b", "main")
    (directory / "f.txt").write_text("x", encoding="utf-8")
    _run_git(directory, "add", ".")
    _run_git(directory, "commit", "-qm", "init")
    base = _run_git(directory, "rev-parse", "HEAD")
    for pr, title in prs:
        _run_git(directory, "checkout", "-q", "-b", f"p{pr}")
        (directory / f"{pr}.txt").write_text(str(pr), encoding="utf-8")
        _run_git(directory, "add", ".")
        _run_git(directory, "commit", "-qm", f"work for #{pr}")
        _run_git(directory, "checkout", "-q", "main")
        _run_git(
            directory,
            "merge", "--no-ff", "-q",
            "-m", f"Merge pull request #{pr} from bllshttng/feature/x-{pr}-thing",
            "-m", title,
            f"p{pr}",
        )
    return base, _run_git(directory, "rev-parse", "HEAD")


def test_release_notes_groups_by_area_and_hides_churn(tmp_path: Path) -> None:
    base, _head = _gh_merge_repo(
        tmp_path,
        [
            (101, "feat(mux): new sidebar"),
            (102, "test(board): cover the sorter"),
            (103, "docs: readme"),
            (104, "fix(agents): stop the crash"),
            (105, "feat(board): card rows"),
            (100, "chore: lint"),
        ],
    )
    notes = update._release_notes(base, tmp_path)
    assert notes is not None
    # Newest first; feats lead.
    assert [ln["pr"] for ln in notes["highlights"]] == [105, 101]
    # Both highlights left their groups; mux emptied, so only agents remains.
    assert [g["area"] for g in notes["groups"]] == ["agents"]
    agents = notes["groups"][0]["lines"]
    assert agents[0]["pr"] == 104
    assert agents[0]["text"] == "stop the crash"
    assert agents[0]["url"] is None
    assert notes["hidden_line"] == "3 test/docs/ci/chore PRs hidden"


def test_release_notes_no_feats_leads_with_first_two_visible(tmp_path: Path) -> None:
    base, _head = _gh_merge_repo(
        tmp_path,
        [
            (201, "fix(mux): pane restore order"),
            (202, "refactor(agents): fold the prober"),
            (203, "test: cover it"),
        ],
    )
    notes = update._release_notes(base, tmp_path)
    assert [ln["pr"] for ln in notes["highlights"]] == [202, 201]
    assert notes["hidden_line"] == "1 test/docs/ci/chore PR hidden"


def test_release_notes_only_churn_shows_chores_group(tmp_path: Path) -> "dict":
    base, _head = _gh_merge_repo(
        tmp_path,
        [
            (301, "test: cover it"),
            (302, "docs: readme"),
        ],
    )
    notes = update._release_notes(base, tmp_path)
    assert [g["area"] for g in notes["groups"]] == ["chores"]
    assert notes["hidden_line"] is None
    # Nothing user-facing changed, so there is nothing to highlight.
    assert notes["highlights"] == []


def test_release_notes_none_on_git_failure() -> None:
    assert update._release_notes("deadbeef", Path("/nonexistent/src")) is None


def test_origin_pr_url_base_reads_origin(tmp_path: Path) -> None:
    _run_git(tmp_path.mkdir(exist_ok=True) or tmp_path, "init", "-q", "-b", "main")
    _run_git(tmp_path, "remote", "add", "origin", "https://github.com/o/r.git")
    assert update._origin_pr_url_base(tmp_path) == "https://github.com/o/r/pull"


def test_origin_pr_url_base_none_without_remote(tmp_path: Path) -> None:
    tmp_path.mkdir(exist_ok=True)
    _run_git(tmp_path, "init", "-q", "-b", "main")
    assert update._origin_pr_url_base(tmp_path) is None
