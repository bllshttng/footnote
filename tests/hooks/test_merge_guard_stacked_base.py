#!/usr/bin/env python3
"""The stacked-base veto inside the gh-pr-merge guard.

Run: python3 tests/hooks/test_merge_guard_stacked_base.py
 or: pytest tests/hooks/test_merge_guard_stacked_base.py

`hooks/git-protection.py` is the only caller of the base-lineage predicate that
sees a BARE `gh pr merge`, and it is wired on both harnesses
(hooks/hooks.json, hooks/codex-hooks.json), so it covers the agents that run gh
through a tool call.

The two properties worth pinning are the ones that would make it decorative:
it must fail OPEN on everything except a confirmed refusal (a guard whose own
machinery is down must not become a merge outage), and it must NOT be buyable
with the merge-gate override marker, which exists to skip the review ceremony
rather than to ship a merge that reaches nobody.
"""
import importlib.util
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
HOOK_PATH = REPO_ROOT / "hooks" / "git-protection.py"

_spec = importlib.util.spec_from_file_location("git_protection", HOOK_PATH)
git_protection = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(git_protection)


class _Proc:
    def __init__(self, returncode, stdout="", stderr=""):
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def _patch_run(monkeypatch, result):
    """Stand in for the `fno pr base-lineage-check` subprocess."""
    seen = {}

    def fake_run(cmd, **kwargs):
        seen["cmd"] = cmd
        seen["timeout"] = kwargs.get("timeout")
        if isinstance(result, Exception):
            raise result
        return result

    monkeypatch.setattr(git_protection.subprocess, "run", fake_run)
    return seen


def test_confirmed_stale_base_refuses(monkeypatch):
    seen = _patch_run(
        monkeypatch,
        _Proc(3, stderr="base-lineage: REFUSED - base branch 'feature/x' already landed\n"),
    )
    msg = git_protection._stacked_base_refusal("gh pr merge 800 --merge")
    assert msg and "already landed" in msg
    assert seen["cmd"] == ["fno", "pr", "base-lineage-check", "800"]
    # Unbounded would let a wedged probe hang the whole tool call.
    assert seen["timeout"]


def test_exit_zero_allows(monkeypatch):
    """Covers both a healthy base and the operator's documented bypass."""
    _patch_run(monkeypatch, _Proc(0, stdout="base-lineage: ok\n"))
    assert git_protection._stacked_base_refusal("gh pr merge 800") is None


def test_unevaluated_probe_does_not_block(monkeypatch):
    """Exit 4 means the check could not run. CI fails closed on that; a merge
    in flight must not, or a gh outage becomes an outage of merging."""
    _patch_run(monkeypatch, _Proc(4, stderr="base-lineage: unknown\n"))
    assert git_protection._stacked_base_refusal("gh pr merge 800") is None


def test_missing_fno_does_not_block(monkeypatch):
    _patch_run(monkeypatch, FileNotFoundError("fno"))
    assert git_protection._stacked_base_refusal("gh pr merge 800") is None


def test_timeout_does_not_block(monkeypatch):
    _patch_run(monkeypatch, subprocess.TimeoutExpired("fno", 90))
    assert git_protection._stacked_base_refusal("gh pr merge 800") is None


def test_unparseable_pr_is_skipped(monkeypatch):
    """The branch-name and current-branch forms name no PR, so there is nothing
    to check. It must skip rather than guess a PR number."""
    calls = _patch_run(monkeypatch, _Proc(3))
    assert git_protection._stacked_base_refusal("gh pr merge my-branch") is None
    assert git_protection._stacked_base_refusal("gh pr merge") is None
    assert "cmd" not in calls


def test_veto_precedes_the_override_marker():
    """The merge-gate override must not buy past a stale base.

    Pinned by source order rather than by running main(): the veto has to sit
    ahead of BOTH the two-factor allow and the marker path, and a test that
    only checked the two-factor path would pass while the marker still shipped
    a merge that reaches nobody.
    """
    src = HOOK_PATH.read_text()
    veto = src.index("_stacked_base_refusal(merge_seg)")
    two_factor = src.index("_check_pr_merge_allowed(merge_seg)")
    marker = src.index("_claim_marker(MERGE_GATE_MARKER)")
    assert veto < two_factor < marker


def _main():
    import pytest

    raise SystemExit(pytest.main([__file__, "-q"]))


if __name__ == "__main__":
    _main()
