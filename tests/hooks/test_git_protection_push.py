#!/usr/bin/env python3
"""Tests for is_push_to_protected_branch's explicit-destination fallthrough.

Run: python3 tests/hooks/test_git_protection_push.py
 or: pytest tests/hooks/test_git_protection_push.py

Regression: is_push_to_protected_branch() ran the current-branch
check unconditionally, so `git push origin feature/x` from a session whose cwd
HEAD is `main` (the normal background /target case: cwd pinned to the canonical
checkout while the branch lives in a worktree) was wrongly blocked as a push to
main. The fix returns early once an explicit, non-protected destination is
parsed. get_current_branch is monkeypatched to "main" to simulate that cwd.
"""
import importlib.util
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
HOOK_PATH = REPO_ROOT / "hooks" / "git-protection.py"

_spec = importlib.util.spec_from_file_location("git_protection", HOOK_PATH)
git_protection = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(git_protection)


def _on_main(monkeypatched_branch="main"):
    git_protection.get_current_branch = lambda: monkeypatched_branch


def test_feature_push_from_cwd_on_main_is_allowed():
    _on_main("main")
    assert git_protection.is_push_to_protected_branch(
        "git push origin feature/foo") == (False, None)


def test_explicit_push_to_main_still_blocked():
    _on_main("feature/x")  # cwd branch is irrelevant; explicit dest is main
    assert git_protection.is_push_to_protected_branch(
        "git push origin main") == (True, "main")


def test_bare_push_on_protected_branch_still_blocked():
    _on_main("main")
    assert git_protection.is_push_to_protected_branch("git push") == (True, "main")


def test_refspec_to_protected_dest_still_blocked():
    _on_main("feature/x")
    assert git_protection.is_push_to_protected_branch(
        "git push origin feature/x:main") == (True, "main")


# The early return must not fire on an ambiguous single-token or current-branch
# push: `extract_branch_from_push` returns the REMOTE ("origin") or "HEAD" as if
# it were a branch, which would otherwise bypass protection on `main`.

def test_remote_only_push_on_main_still_blocked():
    _on_main("main")
    assert git_protection.is_push_to_protected_branch(
        "git push origin") == (True, "main")


def test_force_remote_only_push_on_main_still_blocked():
    _on_main("main")
    assert git_protection.is_push_to_protected_branch(
        "git push --force origin") == (True, "main")


def test_push_head_on_main_still_blocked():
    _on_main("main")
    assert git_protection.is_push_to_protected_branch(
        "git push origin HEAD") == (True, "main")


def test_push_at_alias_on_main_still_blocked():
    _on_main("main")
    assert git_protection.is_push_to_protected_branch(
        "git push origin @") == (True, "main")


def test_upstream_flag_feature_push_still_allowed():
    _on_main("main")
    assert git_protection.is_push_to_protected_branch(
        "git push -u origin feature/x") == (False, None)


# --force-with-lease carries an =<ref> value that must be stripped whole, else
# the leftover token shifts the positional parse and a force-with-lease to a
# protected branch slips through the destination check.

def test_force_with_lease_to_feature_allowed():
    _on_main("feature/x")
    assert git_protection.is_push_to_protected_branch(
        "git push --force-with-lease origin feature/x") == (False, None)


def test_force_with_lease_to_main_still_blocked():
    _on_main("feature/x")
    assert git_protection.is_push_to_protected_branch(
        "git push --force-with-lease origin main") == (True, "main")


def test_force_with_lease_ref_value_to_main_still_blocked():
    _on_main("feature/x")
    assert git_protection.is_push_to_protected_branch(
        "git push --force-with-lease=origin/feature origin main") == (True, "main")


def test_force_with_lease_ref_value_to_feature_allowed():
    _on_main("feature/x")
    assert git_protection.is_push_to_protected_branch(
        "git push --force-with-lease=origin/main origin feature/x") == (False, None)


if __name__ == "__main__":
    for _name, _fn in sorted(globals().items()):
        if _name.startswith("test_") and callable(_fn):
            _fn()
    print("ok: all git-protection push scenarios pass")
