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
    # Unbounded would let a wedged probe hang the whole tool call - and a bound
    # at or above the harness hook budget (60s default) is nearly as bad: the
    # HOOK gets killed, so the two-factor gate this veto sits in front of never
    # runs and emits no verdict at all.
    assert seen["timeout"] and seen["timeout"] < 60


def _main():
    import pytest

    raise SystemExit(pytest.main([__file__, "-q"]))


if __name__ == "__main__":
    _main()
