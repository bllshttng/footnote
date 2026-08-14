"""x-3a3f: the merge gate's missing-row recompute.

A session with no target manifest never runs the stop hook, so no
review_coverage row can exist - the gate was unsatisfiable for that shape.
The merge now fires the standalone producer ONCE and re-reads. These pin the
non-weakening edges: unreviewed still refuses (with or without a working
recompute), a reviewed PR passes only after exactly one recompute, and a
stale head still refuses after one. The hermetic default stub in
cli/tests/conftest.py keeps the verb seam inert; each test here re-pins it.
"""
import json

import pytest

from fno.pr import _merge, _reviews
from fno.pr._proc import Result

from .test_pr_merge import FakeRun, _last_json, enabled  # noqa: F401


def _stub_recompute(monkeypatch, tmp_path, *, coverage, count, head, calls):
    """Replace the verb seam with one that appends a coverage event to the
    project log - the observable effect of the real binary's append."""

    def fake(pr_number, cwd, head_arg):
        calls.append((pr_number, head_arg))
        events = tmp_path / ".fno" / "events.jsonl"
        events.parent.mkdir(exist_ok=True)
        data = {"pr": pr_number, "coverage": coverage, "head_sha": head}
        if coverage in ("covered", "uncovered"):
            data["reviewed_count"] = count
        with open(events, "a", encoding="utf-8") as fh:
            fh.write(
                json.dumps(
                    {"ts": "2026-08-14T03:00:00Z", "type": "review_coverage", "data": data}
                )
                + "\n"
            )
        return True, ""

    monkeypatch.setattr(_reviews, "_fire_review_coverage_verb", fake)
    # Route the gate through the REAL read (the `enabled` fixture's covered
    # stub would bypass the recompute entirely): the only seam is the verb.
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: _reviews.review_coverage_for_gate(pr, repo, head),
    )


def test_recompute_unreviewed_still_refuses(enabled, monkeypatch, capsys, tmp_path):  # noqa: F811
    """Plan test 5: an unreviewed PR with a WORKING recompute recomputes to
    uncovered and the merge still exits 2, with the receipt naming the
    recompute."""
    calls: list = []
    _stub_recompute(monkeypatch, tmp_path, coverage="uncovered", count=0, head="abc", calls=calls)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "blocked"
    assert "uncovered" in obj["reason"], obj["reason"]
    assert "recomputed" in obj["reason"], obj["reason"]
    assert len(calls) == 1, "exactly one recompute"


def test_recompute_unavailable_fails_closed(enabled, monkeypatch, capsys, tmp_path):  # noqa: F811
    """Plan test 6: fno-agents unresolvable -> the refusal keeps today's exit 2
    and names the recompute's absence, not a bare count."""
    monkeypatch.setattr(
        _reviews, "_fire_review_coverage_verb", lambda *a, **k: (False, "fno-agents not found")
    )
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: _reviews.review_coverage_for_gate(pr, repo, head),
    )
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    reason = _last_json(capsys, stream="err")["reason"]
    assert "no review_coverage event" in reason
    assert "recompute unavailable: fno-agents not found" in reason, reason


def test_recompute_reviewed_pr_passes_after_exactly_one(enabled, monkeypatch, capsys, tmp_path):  # noqa: F811
    """Plan test 7: a reviewed PR with no prior event clears the coverage guard
    after ONE recompute - never a loop."""
    (tmp_path / ".fno").mkdir()
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    calls: list = []
    _stub_recompute(monkeypatch, tmp_path, coverage="covered", count=1, head="abc", calls=calls)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"
    assert len(calls) == 1, f"the recompute must fire exactly once, fired {len(calls)}"


def test_recompute_moved_head_still_refuses(enabled, monkeypatch, capsys, tmp_path):  # noqa: F811
    """Plan test 8: a stale row triggers the recompute, and when the
    recomputed event's head still disagrees with the PR head the staleness
    branch refuses exactly as before - a fresh event cannot be manufactured
    for the wrong commit."""
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: "newhead")
    calls: list = []
    _stub_recompute(
        monkeypatch, tmp_path, coverage="covered", count=2, head="otherhead", calls=calls
    )
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    reason = _last_json(capsys, stream="err")["reason"]
    # The receipt names both heads truncated to 8 chars (its long-standing
    # shape); the heads disagreeing is the point, and so is the recompute
    # clause proving a fresh event was attempted and did not clear it.
    assert "otherhea" in reason and "newhead" in reason, reason
    assert "[recomputed]" in reason, reason
    assert len(calls) == 1
