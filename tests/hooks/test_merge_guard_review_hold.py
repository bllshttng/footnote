#!/usr/bin/env python3
"""The in-flight-review veto inside the gh-pr-merge guard.

Run: python3 tests/hooks/test_merge_guard_review_hold.py
 or: pytest tests/hooks/test_merge_guard_review_hold.py

A review that is RUNNING is invisible to every merge gate: coverage answers what
verdicts EXIST for a head, and a review still writing its fixes has produced
none. `fno do pr merge` reads the precise per-branch predicate; this hook cannot,
so it reads the claims directory directly and denies coarsely.

The two invariants pinned here are the ones that would make the veto dangerous.
It must spend NO subprocess budget - the two existing vetoes already take 25s
each against a 60s harness budget with under 6s of margin, and a killed hook
emits no verdict at all - and it must never fail in the ALLOW direction, since
the recovery from a wrong deny is one command and the recovery from a wrong
allow is a revert.
"""
import importlib.util
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
HOOK_PATH = REPO_ROOT / "hooks" / "git-protection.py"

_spec = importlib.util.spec_from_file_location("git_protection", HOOK_PATH)
git_protection = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(git_protection)

MERGE = "gh pr merge 42 --squash"
HOLD_LOCK = "review%3Abranch%3Afeature%2Fx-a089.lock"


class _Proc:
    def __init__(self, returncode, stdout="", stderr=""):
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def _repo_with(tmp_path, lock_names):
    """A fake canonical checkout whose .fno/claims carries `lock_names`."""
    claims = tmp_path / ".fno" / "claims"
    claims.mkdir(parents=True)
    for name in lock_names:
        (claims / name).write_text("holder: reviewer:sess-1\n", encoding="utf-8")
    return tmp_path / ".git"


def _patch_git(monkeypatch, git_dir, *, fail=False):
    calls = []

    def fake_run(cmd, **kwargs):
        calls.append(list(cmd))
        if fail:
            return _Proc(128, "", "fatal: not a git repository")
        return _Proc(0, f"{git_dir}\n", "")

    monkeypatch.setattr(git_protection.subprocess, "run", fake_run)
    return calls


def test_a_registered_review_denies_the_bare_merge(monkeypatch, tmp_path):
    git_dir = _repo_with(tmp_path, [HOLD_LOCK])
    _patch_git(monkeypatch, git_dir)
    refusal = git_protection._review_hold_refusal(MERGE)
    assert refusal is not None
    # The refusal names the branch a human can go look at, decoded from the
    # lockfile name rather than left as percent-escapes.
    assert "review:branch:feature/x-a089" in refusal


def test_an_empty_claims_dir_allows(monkeypatch, tmp_path):
    git_dir = _repo_with(tmp_path, [])
    _patch_git(monkeypatch, git_dir)
    assert git_protection._review_hold_refusal(MERGE) is None


def test_an_unrelated_claim_is_not_a_review_hold(monkeypatch, tmp_path):
    """Only the review keyspace counts: a node claim is not a running review."""
    git_dir = _repo_with(tmp_path, ["node%3Ax-a089.lock", "lane-slot%3A0.lock"])
    _patch_git(monkeypatch, git_dir)
    assert git_protection._review_hold_refusal(MERGE) is None


def test_it_reads_the_common_git_dir_not_the_worktree_one(monkeypatch, tmp_path):
    """A review running in a linked worktree must be visible to a merge
    attempted anywhere in the project, and a repo-local claim lives under the
    canonical checkout."""
    git_dir = _repo_with(tmp_path, [HOLD_LOCK])
    calls = _patch_git(monkeypatch, git_dir)
    git_protection._review_hold_refusal(MERGE)
    assert calls and "--git-common-dir" in calls[0]


def test_it_spends_no_fno_subprocess(monkeypatch, tmp_path):
    """The budget invariant. Two vetoes already take 25s each of a 60s hook
    budget; a third probe would get the hook killed, and a killed hook emits no
    verdict at all."""
    git_dir = _repo_with(tmp_path, [HOLD_LOCK])
    calls = _patch_git(monkeypatch, git_dir)
    git_protection._review_hold_refusal(MERGE)
    assert all(cmd[0] != "fno" for cmd in calls)
    assert len(calls) == 1


def test_a_dead_probe_does_not_become_a_merge_outage(monkeypatch, tmp_path):
    """This veto is additive to two fail-closed gates, so its OWN machinery
    failing must not block every merge; the sanctioned path still reads the
    real predicate."""
    git_dir = _repo_with(tmp_path, [HOLD_LOCK])
    _patch_git(monkeypatch, git_dir, fail=True)
    assert git_protection._review_hold_refusal(MERGE) is None


def test_a_non_merge_command_is_never_vetoed(monkeypatch, tmp_path):
    git_dir = _repo_with(tmp_path, [HOLD_LOCK])
    _patch_git(monkeypatch, git_dir)
    assert git_protection._review_hold_refusal("gh pr create --fill") is None


def test_another_repository_is_out_of_scope(monkeypatch, tmp_path):
    git_dir = _repo_with(tmp_path, [HOLD_LOCK])
    _patch_git(monkeypatch, git_dir)
    assert git_protection._review_hold_refusal(f"{MERGE} --repo other/thing") is None


if __name__ == "__main__":
    sys.exit(pytest.main([__file__, "-q"]))
