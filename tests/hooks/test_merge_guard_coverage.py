#!/usr/bin/env python3
"""The review-coverage veto inside the gh-pr-merge guard.

Run: python3 tests/hooks/test_merge_guard_coverage.py
 or: pytest tests/hooks/test_merge_guard_coverage.py

`_coverage_refusal` consults the merge guard's own coverage predicate through
the hidden `fno pr coverage-check` verb, so a bare `gh pr merge` that the
sanctioned guard would refuse cannot pass this hook on the hook's different
two-factor question. The invariants pinned here are the two that would make
the veto decorative or dangerous: absence REFUSES (an empty read means
nothing attested the head, never that the instrument failed), while every
NAMED instrument failure fails OPEN (exit 4, a missing fno, a timeout,
another repository) so a guard whose own machinery is down does not become a
merge outage. The probe budget assertions keep the two vetoes' worst case
inside the harness's 60s hook budget, because a hook that gets killed emits
no verdict at all.
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


class _Proc:
    def __init__(self, returncode, stdout="", stderr=""):
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def _patch_run(monkeypatch, result):
    """Stand in for the `fno pr coverage-check` subprocess."""
    seen = {}

    def fake_run(cmd, **kwargs):
        seen["cmd"] = cmd
        seen["timeout"] = kwargs.get("timeout")
        if isinstance(result, Exception):
            raise result
        return result

    monkeypatch.setattr(git_protection.subprocess, "run", fake_run)
    return seen


def test_confirmed_refusal_denies(monkeypatch):
    seen = _patch_run(
        monkeypatch,
        _Proc(3, stderr="coverage uncovered: 0 reviewed (no head-pinned pass "
                        "attestation - run the review verb at HEAD)\n"),
    )
    msg = git_protection._coverage_refusal("gh pr merge 900 --squash")
    assert msg and msg.startswith("coverage uncovered: 0 reviewed")
    # No --recompute: the Rust producer is budgeted in minutes and this veto
    # runs inside a 60s PreToolUse budget. Reading is allowed; producing is not.
    assert seen["cmd"] == ["fno", "pr", "coverage-check", "900"]


def test_exit_zero_allows(monkeypatch):
    _patch_run(monkeypatch, _Proc(0, stdout=""))
    assert git_protection._coverage_refusal("gh pr merge 900") is None


def test_unanswered_does_not_block(monkeypatch):
    """Exit 4 is a named instrument failure (head fetch, raised read). A guard
    whose own machinery is down must not become a merge outage."""
    _patch_run(monkeypatch, _Proc(4, stderr="pr head fetch failed"))
    assert git_protection._coverage_refusal("gh pr merge 900") is None


def test_missing_fno_does_not_block(monkeypatch):
    _patch_run(monkeypatch, FileNotFoundError("fno"))
    assert git_protection._coverage_refusal("gh pr merge 900") is None


def test_timeout_does_not_block(monkeypatch):
    _patch_run(monkeypatch, subprocess.TimeoutExpired("fno", 90))
    assert git_protection._coverage_refusal("gh pr merge 900") is None


def test_unparseable_pr_is_skipped(monkeypatch):
    """The branch-name and current-branch forms name no PR, so there is nothing
    to check. It must skip rather than guess a PR number."""
    calls = _patch_run(monkeypatch, _Proc(3))
    assert git_protection._coverage_refusal("gh pr merge my-branch") is None
    assert git_protection._coverage_refusal("gh pr merge") is None
    assert "cmd" not in calls


def test_dispatch_hold_veto_refuses_confirmed_hold(monkeypatch):
    seen = _patch_run(
        monkeypatch,
        _Proc(3, stderr="dispatch-hold:x-5a5c: blocking; set_by=king\n"),
    )
    msg = git_protection._dispatch_hold_refusal("gh pr merge 900")
    assert msg == "dispatch-hold:x-5a5c: blocking; set_by=king"
    assert seen["cmd"] == ["fno", "pr", "hold-check", "900"]
    assert seen["timeout"] <= 5


def test_dispatch_hold_veto_allows_proven_unheld(monkeypatch):
    _patch_run(monkeypatch, _Proc(0, stdout="PR 900: no plan dispatch hold\n"))
    assert git_protection._dispatch_hold_refusal("gh pr merge 900") is None


def test_dispatch_hold_veto_falls_back_to_source_cli(monkeypatch):
    calls = []

    def fake_run(cmd, **kwargs):
        calls.append(cmd)
        if cmd[0] == "fno":
            raise FileNotFoundError("fno")
        return _Proc(0, stdout="PR 900: no plan dispatch hold\n")

    monkeypatch.setattr(git_protection.subprocess, "run", fake_run)
    assert git_protection._dispatch_hold_refusal("gh pr merge 900") is None
    assert calls[1][:4] == [sys.executable, "-m", "fno.cli", "pr"]


@pytest.mark.parametrize(
    "failure",
    [FileNotFoundError("fno"), subprocess.TimeoutExpired("fno", 25)],
)
def test_dispatch_hold_veto_fails_closed_when_probe_unavailable(monkeypatch, failure):
    _patch_run(monkeypatch, failure)
    msg = git_protection._dispatch_hold_refusal("gh pr merge 900")
    assert msg and "refusing to assume unheld" in msg


def test_other_repo_is_skipped(monkeypatch):
    """Every probe reads THIS checkout, so a merge aimed at another repository
    is unanswerable and must not even spawn the subprocess."""
    calls = _patch_run(monkeypatch, _Proc(3))
    assert git_protection._coverage_refusal("gh pr merge 42 --repo other/repo") is None
    assert git_protection._coverage_refusal("gh pr merge 42 -R other/repo") is None
    assert git_protection._coverage_refusal("gh pr merge 42 -Rother/repo") is None
    assert git_protection._coverage_refusal("gh pr merge 42 --repo=other/repo") is None
    assert git_protection._coverage_refusal("GH_REPO=other/repo gh pr merge 42") is None
    assert git_protection._coverage_refusal(
        "gh pr merge https://github.com/other/repo/pull/42"
    ) is None
    assert "cmd" not in calls


def test_flag_values_do_not_disarm_the_veto(monkeypatch):
    """A flag VALUE carrying the override spelling is one argument to gh, not
    a repo override. The whitespace-split matcher read them as overrides and
    silently skipped both vetoes: `-t "-R fixes the null deref"`, a subject
    citing another PR's URL, and a body saying `See /pull/123`."""
    seen = _patch_run(monkeypatch, _Proc(3, stderr="coverage uncovered: 0 reviewed\n"))
    msg = git_protection._coverage_refusal(
        'gh pr merge 42 --squash -t "-R fixes the null deref"'
    )
    assert msg and msg.startswith("coverage uncovered")
    msg = git_protection._coverage_refusal(
        'gh pr merge 42 --subject "see https://github.com/other/repo/pull/9"'
    )
    assert msg and msg.startswith("coverage uncovered")
    # Single-token values carry no space for the quote-aware matcher to lean
    # on, so the override arms must anchor on shape: a subject is not an
    # owner/repo pair and a body word is not a URL.
    msg = git_protection._coverage_refusal('gh pr merge 42 -t "-Rfix"')
    assert msg and msg.startswith("coverage uncovered")
    msg = git_protection._coverage_refusal("gh pr merge 42 --body 'See /pull/123'")
    assert msg and msg.startswith("coverage uncovered")
    msg = git_protection._coverage_refusal("gh pr merge 42 -b see/pull/1")
    assert msg and msg.startswith("coverage uncovered")
    assert seen["cmd"] == ["fno", "pr", "coverage-check", "42"]


def test_stale_binary_fails_open(monkeypatch):
    """A `fno` deployment older than this verb answers an unknown-command
    exit 2, indistinguishable from any other usage error. It fails open (the
    documented rollout-window behavior in `_fno_veto_refusal`) rather than
    turning every stale install into a merge outage."""
    _patch_run(
        monkeypatch,
        _Proc(2, stderr='Error: unknown command "coverage-check" for "fno pr"\n'),
    )
    assert git_protection._coverage_refusal("gh pr merge 900") is None


def test_probe_budgets_fit_the_hook_budget(monkeypatch):
    """Worst case both probes run to their limits. A bound at or above the
    harness hook budget (60s default) is nearly as bad as none: the HOOK gets
    killed, no verdict is emitted, and an unauthorized merge sails through on
    a slow network - so each bound, and their sum, must sit under 60s."""
    timeouts = {}

    def fake_run(cmd, **kwargs):
        timeouts[cmd[2]] = kwargs.get("timeout")
        return _Proc(0)

    monkeypatch.setattr(git_protection.subprocess, "run", fake_run)
    git_protection._stacked_base_refusal("gh pr merge 800")
    git_protection._coverage_refusal("gh pr merge 800")
    assert timeouts["base-lineage-check"] < 60
    assert timeouts["coverage-check"] < 60
    assert timeouts["base-lineage-check"] + timeouts["coverage-check"] < 60


def test_veto_precedes_the_two_factor_allow_and_the_override_marker():
    """Pinned by source order rather than by running main(): the veto has to
    sit ahead of BOTH the two-factor allow and the marker path, because the
    override marker buys out review ceremony and an unreviewed head is not
    ceremony. A test that only checked the two-factor path would pass while
    the marker still shipped an unreviewed merge."""
    src = HOOK_PATH.read_text()
    stacked = src.index("_stacked_base_refusal(merge_seg)")
    coverage = src.index("_coverage_refusal(merge_seg)")
    two_factor = src.index("_check_pr_merge_allowed(merge_seg)")
    marker = src.index("_claim_marker(MERGE_GATE_MARKER)")
    assert stacked < coverage < two_factor < marker


def _main():
    import pytest

    raise SystemExit(pytest.main([__file__, "-q"]))


if __name__ == "__main__":
    _main()
