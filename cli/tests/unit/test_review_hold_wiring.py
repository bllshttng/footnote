"""The review hold is taken and cleared by footnote's own code, not by memory.

`fno do pr merge` is fail-closed against the hold, and the hold works. What
failed on PR 1575 is that nothing took it: the merge fired while the only
non-author review was still running, and eight findings (one HIGH) were
discarded. A hold nobody takes is identical to a hold that does not exist.

The requester verb takes it; the attestation clears it, because a verdict for
this head now exists and merge readiness can see the review.

Nothing here counts a round or gates a merge on origin (laws d-0fa92eb9,
d-777e7d1f). A hold says a review is RUNNING, and that is all.
"""

import inspect
from pathlib import Path

import pytest

from fno import target_cli


@pytest.fixture()
def taken(monkeypatch):
    calls: list[dict] = []
    monkeypatch.setattr(
        "fno.pr._review_hold.acquire_review_hold",
        lambda branch, **kw: calls.append({"branch": branch, **kw}),
    )
    monkeypatch.setattr("fno.pr._review_hold.review_invocation_refusal", lambda *a, **kw: "")
    monkeypatch.setattr(target_cli, "_git_out", lambda cwd, *args: "feature/x-5ca3")
    return calls


def test_a_sent_review_request_takes_the_hold(taken) -> None:
    target_cli._hold_branch_under_review(
        Path("/repo"),
        head_sha="deadbeef" * 5,
        session_id="sess-1",
        receipt={"outcome": "queued"},
    )

    assert len(taken) == 1
    assert taken[0]["branch"] == "feature/x-5ca3"
    assert taken[0]["head"] == "deadbeef" * 5
    assert taken[0]["holder"] == "review-session:sess-1"


@pytest.mark.parametrize("outcome", ["refused", "unconfirmed"])
def test_an_unsent_request_takes_nothing(taken, outcome: str) -> None:
    """No review is running, so nothing may claim one is."""
    target_cli._hold_branch_under_review(
        Path("/repo"),
        head_sha="abc",
        session_id="sess-1",
        receipt={"outcome": outcome},
    )

    assert taken == []


def test_a_detached_head_takes_nothing(taken, monkeypatch) -> None:
    monkeypatch.setattr(target_cli, "_git_out", lambda cwd, *args: "HEAD")

    target_cli._hold_branch_under_review(
        Path("/repo"), head_sha="abc", session_id="s", receipt={"outcome": "started"}
    )

    assert taken == []


def test_a_refused_invocation_takes_nothing(taken, monkeypatch) -> None:
    """The deadlock this avoids, and why the gate is here rather than skipped.

    A refused invocation runs no review and emits no attestation, so nothing
    would ever release the hold. The merge the refusal is telling the worker to
    take would then be blocked for the full TTL by a review that never started.
    """
    monkeypatch.setattr(
        "fno.pr._review_hold.review_invocation_refusal",
        lambda *a, **kw: "review rounds spent: 2 of 2",
    )

    target_cli._hold_branch_under_review(
        Path("/repo"), head_sha="abc", session_id="s", receipt={"outcome": "queued"}
    )

    assert taken == []


def test_a_lockfile_failure_never_refuses_the_sent_review(monkeypatch) -> None:
    def _boom(branch, **kw):
        raise OSError("claims root read-only")

    monkeypatch.setattr("fno.pr._review_hold.acquire_review_hold", _boom)
    monkeypatch.setattr("fno.pr._review_hold.review_invocation_refusal", lambda *a, **kw: "")
    monkeypatch.setattr(target_cli, "_git_out", lambda cwd, *args: "feature/x")

    # Returns, does not raise: the review was already sent.
    target_cli._hold_branch_under_review(
        Path("/repo"), head_sha="abc", session_id="s", receipt={"outcome": "queued"}
    )


def test_the_python_attest_path_releases_the_hold_after_the_row_lands(
    tmp_path: Path, monkeypatch
) -> None:
    """The shell producer released here from the start; this one did not.

    Order is the invariant, not the presence of a call: release must FOLLOW
    the append. A release that ran first would clear the lane on a crashed
    emit, leaving no verdict behind it and no hold in front of it.
    """
    from fno.review import cli as review_cli

    calls: list[str] = []

    git = {
        ("symbolic-ref", "--short", "refs/remotes/origin/HEAD"): "origin/main",
        ("merge-base", "HEAD", "origin/main"): "base0000",
        ("diff", "--name-only", "base0000..HEAD"): "a.py\n",
        ("diff", "--numstat", "base0000..HEAD"): "3\t1\ta.py\n",
    }
    monkeypatch.setattr(review_cli, "_git_out", lambda *args: git.get(tuple(args), ""))
    monkeypatch.setattr(
        "fno.review.invocation._settle_head_pin", lambda cwd: ("head1234", "feature/x-5ca3")
    )
    monkeypatch.setattr(
        "fno.harness_identity.resolve_attester_identity", lambda: ("sess-a", "witness")
    )
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda *a, **kw: tmp_path)
    monkeypatch.setattr("fno.paths.project_log", lambda *a, **kw: tmp_path / "events.jsonl")
    monkeypatch.setattr(
        "fno.events.append_event", lambda *a, **kw: calls.append("append")
    )
    monkeypatch.setattr("fno.events.cli.mirror_to_global_log", lambda *a, **kw: None)
    monkeypatch.setattr(
        "fno.pr._review_hold.release_review_hold",
        lambda branch, **kw: calls.append(f"release:{branch}"),
    )

    verdict = review_cli._attest_from_record(
        {"findings_blocking": 0, "findings_nonblocking": 0, "findings": []},
        "code-review",
        "non-author",
        tmp_path / "findings.json",
    )

    assert verdict == "pass"
    assert calls == ["append", "release:feature/x-5ca3"]
