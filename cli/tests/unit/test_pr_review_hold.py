"""The in-flight-review guard: a running review must block a merge.

Merge readiness models a review as a RECORDED VERDICT, so a review that is
EXECUTING is invisible to it. These tests pin both layers of the answer: the
registered hold (a TTL claim) and the derived worktree probe that catches a
review nothing registered.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.claims.core import acquire_claim
from fno.pr import _review_hold
from fno.pr._proc import Result, ToolMissing


DEAD_PID = 2**30  # far above any live pid; is_live() reads it as dead


def _fake_git(responses):
    """A ``_proc.run`` stand-in keyed on the first distinguishing argument."""

    def runner(cmd, *, cwd=None, env=None, input_text=None, timeout=None):
        key = next((part for part in cmd if part in responses), None)
        if key is None:
            raise AssertionError(f"unexpected command: {cmd}")
        value = responses[key]
        if isinstance(value, Exception):
            raise value
        return value

    return runner


def _worktree_list(entries):
    """Render ``git worktree list --porcelain`` output for (path, head, branch)."""
    blocks = []
    for path, head, branch in entries:
        block = f"worktree {path}\nHEAD {head}\n"
        block += "detached\n" if branch is None else f"branch refs/heads/{branch}\n"
        blocks.append(block)
    return Result(returncode=0, stdout="\n".join(blocks) + "\n", stderr="")


NO_WORKTREE = _fake_git({"worktree": _worktree_list([])})


def test_key_is_branch_scoped_and_repo_local():
    key = _review_hold.review_hold_key("feature/x-a089")
    assert key == "review:branch:feature/x-a089"
    # A repo-local key: the global-id prefixes route to $HOME, and a review
    # hold must live beside the repo whose merge it guards.
    from fno.claims.io import claims_root_for

    assert claims_root_for(key) is None


def test_free_hold_and_no_worktree_is_clear(tmp_path: Path):
    activity = _review_hold.review_activity(
        "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=NO_WORKTREE
    )
    assert activity.blocked is False
    assert activity.blocker == ""
    assert activity.hold is None
    # AC7: the enumeration RAN and answered. That is what makes an absent hold
    # a clear verdict rather than an unanswered one.
    assert activity.worktree["probed"] is True
    assert activity.worktree["path"] is None


def test_live_hold_blocks_with_review_in_flight(tmp_path: Path):
    acquire_claim(
        _review_hold.review_hold_key("feature/x"),
        "reviewer:sess-1",
        ttl_ms=600_000,
        metadata={"head_sha": "abc123", "verb": "/code-review"},
        root=tmp_path,
    )
    activity = _review_hold.review_activity(
        "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=NO_WORKTREE
    )
    assert activity.blocked is True
    assert activity.blocker == "review_in_flight"
    assert activity.hold is not None
    assert activity.hold["holder"] == "reviewer:sess-1"
    assert "reviewer:sess-1" in activity.detail


def test_hold_from_a_dead_pid_inside_its_ttl_still_blocks(tmp_path: Path):
    """SUSPECT is the respawned-reviewer case; the TTL still protects the slot."""
    acquire_claim(
        _review_hold.review_hold_key("feature/x"),
        "reviewer:sess-1",
        ttl_ms=600_000,
        pid=DEAD_PID,
        root=tmp_path,
    )
    activity = _review_hold.review_activity(
        "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=NO_WORKTREE
    )
    assert activity.blocked is True
    assert activity.blocker == "review_in_flight"


def test_corrupted_hold_blocks_rather_than_assuming_unheld(tmp_path: Path):
    from fno.claims.io import claim_path

    path = claim_path(_review_hold.review_hold_key("feature/x"), root=tmp_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{ this is not a claim", encoding="utf-8")
    activity = _review_hold.review_activity(
        "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=NO_WORKTREE
    )
    assert activity.blocked is True
    assert activity.blocker == "review_hold_unreadable"
    assert "refusing to assume unheld" in activity.detail


def test_expired_hold_clears_but_never_silently(tmp_path: Path, monkeypatch, capsys):
    """AC3: a stale hold ages out WITH a receipt. Silence fails this test."""
    acquire_claim(
        _review_hold.review_hold_key("feature/x"),
        "reviewer:sess-1",
        ttl_ms=60_000,
        pid=DEAD_PID,
        root=tmp_path,
    )
    from fno.claims import staleness

    monkeypatch.setattr(staleness, "now_ms", lambda: 99_999_999_999_999)
    emitted: list[dict] = []
    monkeypatch.setattr(_review_hold, "_emit_expired", lambda **kw: emitted.append(kw))

    activity = _review_hold.review_activity(
        "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=NO_WORKTREE
    )
    assert activity.blocked is False
    assert emitted and emitted[0]["holder"] == "reviewer:sess-1"
    assert "expired" in capsys.readouterr().err


def test_tracked_modifications_block_the_merge(tmp_path: Path):
    runner = _fake_git(
        {
            "worktree": _worktree_list([("/wt/x", "abc123", "feature/x")]),
            "status": Result(returncode=0, stdout=" M cli/src/fno/pr/_merge.py\n", stderr=""),
        }
    )
    activity = _review_hold.review_activity(
        "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=runner
    )
    assert activity.blocked is True
    assert activity.blocker == "worktree_dirty"
    assert "/wt/x" in activity.detail


def test_untracked_scratch_never_blocks(tmp_path: Path):
    """A worker's worktree always carries untracked scratch; it is not evidence."""
    seen: list[list[str]] = []

    def runner(cmd, *, cwd=None, env=None, input_text=None, timeout=None):
        seen.append(list(cmd))
        if "worktree" in cmd:
            return _worktree_list([("/wt/x", "abc123", "feature/x")])
        return Result(returncode=0, stdout="", stderr="")

    activity = _review_hold.review_activity(
        "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=runner
    )
    assert activity.blocked is False
    status_cmd = next(cmd for cmd in seen if "status" in cmd)
    assert "--untracked-files=no" in status_cmd


def test_worktree_head_mismatch_blocks(tmp_path: Path):
    runner = _fake_git(
        {
            "worktree": _worktree_list([("/wt/x", "localsha", "feature/x")]),
            "status": Result(returncode=0, stdout="", stderr=""),
        }
    )
    activity = _review_hold.review_activity(
        "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=runner
    )
    assert activity.blocked is True
    assert activity.blocker == "worktree_head_mismatch"
    assert "localsha" in activity.detail and "abc123" in activity.detail


def test_head_conjunct_is_skipped_without_a_pr_head(tmp_path: Path):
    runner = _fake_git(
        {
            "worktree": _worktree_list([("/wt/x", "localsha", "feature/x")]),
            "status": Result(returncode=0, stdout="", stderr=""),
        }
    )
    activity = _review_hold.review_activity(
        "feature/x", pr_head="", repo=str(tmp_path), root=tmp_path, runner=runner
    )
    assert activity.blocked is False


@pytest.mark.parametrize(
    "failure",
    [
        Result(returncode=128, stdout="", stderr="fatal: not a git repository"),
        ToolMissing("git"),
    ],
)
def test_a_dead_probe_blocks_rather_than_clearing(tmp_path: Path, failure):
    runner = _fake_git({"worktree": failure})
    activity = _review_hold.review_activity(
        "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=runner
    )
    assert activity.blocked is True
    assert activity.blocker == "worktree_probe_failed"
    assert activity.worktree["probed"] is False


def test_a_status_probe_failure_blocks(tmp_path: Path):
    runner = _fake_git(
        {
            "worktree": _worktree_list([("/wt/x", "abc123", "feature/x")]),
            "status": Result(returncode=128, stdout="", stderr="fatal: bad object"),
        }
    )
    activity = _review_hold.review_activity(
        "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=runner
    )
    assert activity.blocked is True
    assert activity.blocker == "worktree_probe_failed"


def test_a_detached_or_other_branch_worktree_is_not_this_pr(tmp_path: Path):
    runner = _fake_git(
        {
            "worktree": _worktree_list(
                [("/wt/other", "zzz", "feature/other"), ("/wt/det", "yyy", None)]
            ),
        }
    )
    activity = _review_hold.review_activity(
        "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=runner
    )
    assert activity.blocked is False
    assert activity.worktree["path"] is None


def test_the_hold_is_read_before_the_worktree_probe(tmp_path: Path):
    """Layer 1 answers first: a held branch never pays for the git probe."""
    acquire_claim(
        _review_hold.review_hold_key("feature/x"),
        "reviewer:sess-1",
        ttl_ms=600_000,
        root=tmp_path,
    )

    def explode(cmd, **_kw):
        raise AssertionError("the worktree probe must not run behind a live hold")

    activity = _review_hold.review_activity(
        "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=explode
    )
    assert activity.blocker == "review_in_flight"


def test_release_clears_the_hold(tmp_path: Path):
    key = _review_hold.review_hold_key("feature/x")
    _review_hold.acquire_review_hold(
        "feature/x", head="abc123", holder="reviewer:sess-1", root=tmp_path
    )
    assert (
        _review_hold.review_activity(
            "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=NO_WORKTREE
        ).blocked
        is True
    )
    _review_hold.release_review_hold("feature/x", holder="reviewer:sess-1", root=tmp_path)
    from fno.claims.core import claim_status

    assert claim_status(key, root=tmp_path)["state"] == "free"


def test_acquire_records_the_head_it_is_reviewing(tmp_path: Path):
    claim = _review_hold.acquire_review_hold(
        "feature/x", head="abc123", holder="reviewer:sess-1", verb="/code-review", root=tmp_path
    )
    assert claim is not None
    assert claim.metadata["head_sha"] == "abc123"
    assert claim.metadata["verb"] == "/code-review"


def test_ttl_is_clamped_into_the_claim_bounds(tmp_path: Path):
    """A bad TTL must not make the gate unreadable; it coerces."""
    from fno.claims.types import MAX_TTL_MS, MIN_TTL_MS

    assert _review_hold.resolve_ttl_ms(0) == MIN_TTL_MS
    assert _review_hold.resolve_ttl_ms(10**12) == MAX_TTL_MS
    assert _review_hold.resolve_ttl_ms(None) == _review_hold.DEFAULT_HOLD_TTL_MINUTES * 60_000


def test_refusal_names_the_clear_command(tmp_path: Path):
    acquire_claim(
        _review_hold.review_hold_key("feature/x"),
        "reviewer:sess-1",
        ttl_ms=600_000,
        root=tmp_path,
    )
    refusal = _review_hold.review_hold_refusal(
        "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=NO_WORKTREE
    )
    assert refusal is not None
    # A refusal a reader cannot clear in one command is how a guard becomes a
    # wedge, so the command is part of the contract.
    assert "fno do pr review-hold release" in refusal


def test_no_refusal_when_clear(tmp_path: Path):
    assert (
        _review_hold.review_hold_refusal(
            "feature/x", pr_head="abc123", repo=str(tmp_path), root=tmp_path, runner=NO_WORKTREE
        )
        is None
    )
