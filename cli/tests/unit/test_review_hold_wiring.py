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


def test_a_lockfile_failure_never_refuses_the_sent_review(monkeypatch) -> None:
    def _boom(branch, **kw):
        raise OSError("claims root read-only")

    monkeypatch.setattr("fno.pr._review_hold.acquire_review_hold", _boom)
    monkeypatch.setattr(target_cli, "_git_out", lambda cwd, *args: "feature/x")

    # Returns, does not raise: the review was already sent.
    target_cli._hold_branch_under_review(
        Path("/repo"), head_sha="abc", session_id="s", receipt={"outcome": "queued"}
    )


def test_the_python_attest_path_releases_the_hold() -> None:
    """The shell producer released here from the start; this one did not.

    Read as source rather than executed: `_attest_from_record` needs a git
    repo, an origin base, a non-empty diff and an identity resolver, and the
    ordering is what matters - the release must follow the append, never
    precede it, or a crashed emit clears a hold with no verdict behind it.
    """
    from fno.review.cli import _attest_from_record

    body = inspect.getsource(_attest_from_record)

    assert "release_review_hold(branch)" in body
    assert body.index("append_event(event, events_path=events_path)") < body.index(
        "release_review_hold(branch)"
    )
