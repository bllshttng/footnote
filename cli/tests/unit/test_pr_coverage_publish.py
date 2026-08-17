"""publish_coverage_status: the coverage verdict as a commit status.

The status is the SERVER half of the merge coverage guard: the repo ruleset
requires the context, and this publisher is what writes it. The verdict comes
from the one shared predicate (``_coverage_gate.coverage_verdict``), patched
per test - never a second copy of the conjunction - so the status check can
never disagree with the gate it certifies.
"""
from __future__ import annotations

import pytest

from fno.pr import _coverage_gate, _reviews
from fno.pr._proc import Result

HEAD = "1111111111111111111111111111111111111111"
ROW_HEAD = "2222222222222222222222222222222222222222"


class RecordingRunner:
    """Serves every gh call green and records the argv of each."""

    def __init__(self, *, ok: bool = True, stderr: str = "") -> None:
        self.calls: list[list[str]] = []
        self.ok = ok
        self.stderr = stderr

    def __call__(self, cmd, *, cwd=None, timeout=None):
        self.calls.append(list(cmd))
        return Result(0 if self.ok else 1, "", self.stderr)


def _posts(runner: RecordingRunner) -> list[list[str]]:
    return [c for c in runner.calls if c[:4] == ["gh", "api", "--method", "POST"]]


def _fields(call: list[str]) -> dict:
    out: dict = {}
    for i in range(len(call) - 1):
        if call[i] == "-f":
            key, _, value = call[i + 1].partition("=")
            out[key] = value
    return out


@pytest.fixture
def hermetic(monkeypatch):
    """Every spawn and verdict steered off the network: a recording runner, a
    resolved head, no override label, and a verdict the test sets per case."""
    runner = RecordingRunner()
    monkeypatch.setattr(_reviews, "run", runner)
    monkeypatch.setattr("fno.pr._merge._pr_head_oid", lambda pr, repo: HEAD)
    monkeypatch.setattr(
        _reviews, "_override_label_actor", lambda pr, repo, r: (False, None)
    )
    verdict_box: dict = {}

    def fake_verdict(pr, repo, *, recompute):
        verdict_box["recompute"] = recompute
        return verdict_box["return"]

    monkeypatch.setattr(_coverage_gate, "coverage_verdict", fake_verdict)
    monkeypatch.setattr(
        _reviews, "latest_review_coverage", lambda pr, cwd=None: None
    )
    verdict_box["return"] = (_coverage_gate.COVERED, "", ROW_HEAD, "")
    return runner, verdict_box


def test_a_covered_row_posts_success_on_the_head_the_row_pins(hermetic, monkeypatch):
    runner, _box = hermetic
    # Serve the count the description names.
    monkeypatch.setattr(
        _reviews,
        "latest_review_coverage",
        lambda pr, cwd=None: {
            "coverage": "covered",
            "reviewed_count": 2,
            "head_sha": ROW_HEAD,
        },
    )
    posted, note = _reviews.publish_coverage_status(42)
    assert (posted, note) == (True, "")
    posts = _posts(runner)
    assert len(posts) == 1
    assert ROW_HEAD in posts[0][4]
    fields = _fields(posts[0])
    assert fields["state"] == "success"
    assert fields["context"] == _reviews.COVERAGE_STATUS_CONTEXT
    assert fields["description"] == f"covered: 2 reviewed at {ROW_HEAD[:8]}"


def test_a_covered_row_with_an_unreadable_count_still_names_the_sha(hermetic):
    runner, _box = hermetic
    posted, _note = _reviews.publish_coverage_status(42)
    assert posted is True
    fields = _fields(_posts(runner)[0])
    assert fields["state"] == "success"
    assert fields["description"] == f"covered at {ROW_HEAD[:8]}"


def test_no_review_lane_posts_an_explicit_ungated_success(hermetic):
    runner, verdict_box = hermetic
    verdict_box["return"] = (_coverage_gate.COVERED, "", "", "")
    posted, _note = _reviews.publish_coverage_status(42)
    assert posted is True
    posts = _posts(runner)
    # No row head to pin: the POST lands on the resolved PR head.
    assert HEAD in posts[0][4]
    fields = _fields(posts[0])
    assert fields["state"] == "success"
    assert fields["description"] == "no review lane configured; merge ungated"


def test_a_refusal_posts_the_gate_refusal_text_truncated(hermetic):
    runner, verdict_box = hermetic
    verdict_box["return"] = (
        _coverage_gate.REFUSED,
        "waiting on " + "chatgpt-codex-connector " * 10,
        "",
        "recompute ran",
    )
    posted, _note = _reviews.publish_coverage_status(42)
    assert posted is True
    fields = _fields(_posts(runner)[0])
    assert fields["state"] == "failure"
    # The exact sentence the merge gate renders (reason, bracket-appended
    # note), head-kept under GitHub's description cap.
    assert fields["description"].startswith("waiting on")
    assert len(fields["description"]) <= _reviews._GH_DESCRIPTION_LIMIT


def test_an_unanswered_probe_posts_the_note_as_failure(hermetic):
    runner, verdict_box = hermetic
    verdict_box["return"] = (_coverage_gate.UNANSWERED, "", "", "pr head fetch failed")
    posted, _note = _reviews.publish_coverage_status(42)
    assert posted is True
    fields = _fields(_posts(runner)[0])
    assert fields["state"] == "failure"
    assert fields["description"] == "pr head fetch failed"


def test_the_override_label_wins_before_the_verdict(hermetic, monkeypatch):
    runner, verdict_box = hermetic
    monkeypatch.setattr(
        _reviews, "_override_label_actor", lambda pr, repo, r: (True, "jane")
    )
    verdict_box["return"] = (_coverage_gate.REFUSED, "0 reviewed", "", "")
    posted, _note = _reviews.publish_coverage_status(42)
    assert posted is True
    fields = _fields(_posts(runner)[0])
    assert fields["state"] == "success"
    assert "coverage-override" in fields["description"]
    assert "jane" in fields["description"]


def test_no_resolvable_head_never_posts(hermetic, monkeypatch):
    runner, _box = hermetic
    monkeypatch.setattr("fno.pr._merge._pr_head_oid", lambda pr, repo: None)
    posted, note = _reviews.publish_coverage_status(42)
    assert posted is False
    assert "no PR head" in note
    assert _posts(runner) == []


def test_a_failed_post_is_reported_and_never_raises(hermetic, monkeypatch):
    monkeypatch.setattr(
        _reviews,
        "run",
        RecordingRunner(ok=False, stderr="gh exploded"),
    )
    posted, note = _reviews.publish_coverage_status(42)
    assert posted is False
    assert "gh exploded" in note


def test_the_verdict_is_read_without_recompute(hermetic):
    runner, verdict_box = hermetic
    posted, _note = _reviews.publish_coverage_status(42)
    assert posted is True
    assert verdict_box["recompute"] is False, (
        "a publisher must never spawn the producer subprocess"
    )
