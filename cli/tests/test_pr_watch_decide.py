"""Tests for the PR-state watcher core: decide() matrix and discovery + read error path.

Task 1.1: pure core + read layer. No I/O inside decide(); runner injectable for
read_pr_state tests.

Run: uv run --project cli pytest cli/tests/test_pr_watch_decide.py -q
"""
from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Optional
from unittest.mock import MagicMock

import pytest

# These imports will fail until we implement the module -- that is the RED phase.
from fno.pr_watch import Decision, decide
from fno.pr_watch._discover import (
    PrCandidate,
    PrObservation,
    discover_open_prs,
    read_pr_state,
)
from fno.graph._reconcile import ReconcileError


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _obs(
    *,
    pr_number: int = 42,
    state: str = "OPEN",
    merged: bool = False,  # kept for call-site compatibility; ignored (field removed)
    latest_review_ts: Optional[str] = None,
    opened_at: Optional[str] = "2026-06-01T00:00:00Z",
) -> PrObservation:
    return PrObservation(
        pr_number=pr_number,
        state=state,  # type: ignore[arg-type]
        latest_review_ts=latest_review_ts,
        opened_at=opened_at,
    )


def _watermark(**kwargs) -> dict:
    return dict(kwargs)


NOW = "2026-06-14T12:00:00Z"


# ---------------------------------------------------------------------------
# AC1-HP: new reviewer activity fires "review"
# ---------------------------------------------------------------------------

def test_ac1_hp_new_review_activity_fires_review():
    """AC1-HP: reviewer activity timestamp T+1 > watermark T, reviewers non-empty
    -> Decision.kind == 'review'."""
    obs = _obs(latest_review_ts="2026-06-14T10:00:00Z")
    wm = _watermark(last_review_ts="2026-06-13T10:00:00Z")
    d = decide(obs, watermark=wm, reviewers=["codex"], merge_ready=False, now_iso=NOW)
    assert d.kind == "review"
    assert d.pr_number == 42


# ---------------------------------------------------------------------------
# AC1-EDGE: latest activity == watermark -> noop
# ---------------------------------------------------------------------------

def test_ac1_edge_same_timestamp_is_noop():
    """AC1-EDGE: latest_review_ts equal to watermark does not fire review."""
    obs = _obs(latest_review_ts="2026-06-13T10:00:00Z")
    wm = _watermark(last_review_ts="2026-06-13T10:00:00Z")
    d = decide(obs, watermark=wm, reviewers=["codex"], merge_ready=False, now_iso=NOW)
    assert d.kind == "noop"


def test_ac1_edge_earlier_timestamp_is_noop():
    """AC1-EDGE: latest_review_ts older than watermark does not fire review."""
    obs = _obs(latest_review_ts="2026-06-12T10:00:00Z")
    wm = _watermark(last_review_ts="2026-06-13T10:00:00Z")
    d = decide(obs, watermark=wm, reviewers=["codex"], merge_ready=False, now_iso=NOW)
    assert d.kind == "noop"


# ---------------------------------------------------------------------------
# AC2-HP: MERGED + merge_ready -> "merge"
# ---------------------------------------------------------------------------

def test_ac2_hp_merged_and_ready_fires_merge():
    """AC2-HP: state MERGED, merge_ready True, not yet dispatched -> 'merge'."""
    obs = _obs(state="MERGED", merged=True)
    d = decide(obs, watermark={}, reviewers=[], merge_ready=True, now_iso=NOW)
    assert d.kind == "merge"
    assert d.pr_number == 42


def test_ac2_hp_merged_already_dispatched_is_noop():
    """AC2-HP: merge already dispatched -> noop (idempotency guard)."""
    obs = _obs(state="MERGED", merged=True)
    wm = _watermark(merge_dispatched=True)
    d = decide(obs, watermark=wm, reviewers=[], merge_ready=True, now_iso=NOW)
    assert d.kind == "noop"


def test_post_merge_verdict_is_ready_seam():
    """Regression: the tick reads `post_merge_readiness(...).is_ready`; a real
    verdict must expose that boolean (the missing property raised AttributeError
    every tick and silently forced merge_ready=False)."""
    from fno.config_cli import PostMergeVerdict

    ready = PostMergeVerdict(status="ready", enabled=True, activity=True)
    assert ready.is_ready is True
    for status in ("unconfigured", "opted_out", "dormant", "error"):
        assert PostMergeVerdict(status=status, enabled=True, activity=False).is_ready is False

    # And a merged PR with a ready verdict reaches a `merge` decision through
    # the boolean the tick actually reads.
    obs = _obs(state="MERGED", merged=True)
    d = decide(obs, watermark={}, reviewers=[], merge_ready=ready.is_ready, now_iso=NOW)
    assert d.kind == "merge"


# ---------------------------------------------------------------------------
# AC2-FR: CLOSED (not merged) -> "park"
# ---------------------------------------------------------------------------

def test_ac2_fr_closed_not_merged_parks():
    """AC2-FR: state CLOSED (not merged) -> 'park', reason 'closed'."""
    obs = _obs(state="CLOSED", merged=False)
    d = decide(obs, watermark={}, reviewers=[], merge_ready=False, now_iso=NOW)
    assert d.kind == "park"
    assert d.reason == "closed"


# ---------------------------------------------------------------------------
# Boundary: empty entries -> empty list
# ---------------------------------------------------------------------------

def test_boundary_empty_entries_returns_empty():
    """Boundary: empty graph entries list -> discover_open_prs returns []."""
    result = discover_open_prs([])
    assert result == []


def test_boundary_node_with_completed_at_excluded():
    """Boundary: node with completed_at set is excluded from open-PR discovery."""
    node = {
        "id": "x-0001",
        "pr_number": 10,
        "pr_url": "https://github.com/owner/repo/pull/10",
        "completed_at": "2026-06-13T00:00:00Z",
        "cwd": "/tmp/repo",
    }
    result = discover_open_prs([node])
    assert result == []


def test_done_node_within_grace_window_discovered(tmp_path):
    """AC1-EDGE / Wave 2: a node done-at-PR-green within max_age_days is still
    watched (bridges the PR-green -> merge gap) when a clock is supplied."""
    repo_dir = tmp_path / "repo"
    repo_dir.mkdir()
    (repo_dir / ".git").mkdir()
    node = {
        "id": "x-0010",
        "pr_number": 11,
        "pr_url": "https://github.com/owner/repo/pull/11",
        "completed_at": "2026-06-13T18:02:00Z",  # 1 day before NOW
        "cwd": str(repo_dir),
    }
    result = discover_open_prs([node], now_iso=NOW, max_age_days=14)
    assert [c.node_id for c in result] == ["x-0010"]


def test_done_node_outside_grace_window_excluded(tmp_path):
    """AC1-EDGE / Wave 2: a node completed > max_age_days ago is dropped."""
    repo_dir = tmp_path / "repo"
    repo_dir.mkdir()
    (repo_dir / ".git").mkdir()
    node = {
        "id": "x-0011",
        "pr_number": 12,
        "pr_url": "https://github.com/owner/repo/pull/12",
        "completed_at": "2026-05-01T00:00:00Z",  # > 14 days before NOW
        "cwd": str(repo_dir),
    }
    assert discover_open_prs([node], now_iso=NOW, max_age_days=14) == []


def test_superseded_done_node_never_discovered(tmp_path):
    """AC1-EDGE / Wave 2: a superseded node is excluded even inside the window."""
    repo_dir = tmp_path / "repo"
    repo_dir.mkdir()
    (repo_dir / ".git").mkdir()
    node = {
        "id": "x-0012",
        "pr_number": 13,
        "pr_url": "https://github.com/owner/repo/pull/13",
        "completed_at": "2026-06-13T00:00:00Z",
        "superseded_by": "x-0099",
        "cwd": str(repo_dir),
    }
    assert discover_open_prs([node], now_iso=NOW, max_age_days=14) == []


def test_boundary_node_with_no_pr_number_excluded():
    """Boundary: node with no pr_number is excluded."""
    node = {
        "id": "x-0002",
        "pr_number": None,
        "cwd": "/tmp/repo",
    }
    result = discover_open_prs([node])
    assert result == []


def test_boundary_open_node_with_pr_number_included(tmp_path):
    """Boundary: open node with pr_number produces a PrCandidate."""
    repo_dir = tmp_path / "repo"
    repo_dir.mkdir()
    (repo_dir / ".git").mkdir()
    node = {
        "id": "x-0003",
        "pr_number": 99,
        "pr_url": "https://github.com/owner/repo/pull/99",
        "cwd": str(repo_dir),
    }
    result = discover_open_prs([node])
    assert len(result) == 1
    cand = result[0]
    assert cand.node_id == "x-0003"
    assert cand.pr_number == 99
    assert cand.repo_slug == "owner/repo"


def test_source_session_id_threads_onto_candidate(tmp_path):
    """The node's originating session (warm-route target) reaches the candidate;
    a node without one is None (a field rename would silently break warm routing)."""
    repo_dir = tmp_path / "repo"
    repo_dir.mkdir()
    (repo_dir / ".git").mkdir()
    with_sess = {
        "id": "x-0006", "pr_number": 5,
        "pr_url": "https://github.com/owner/repo/pull/5",
        "cwd": str(repo_dir), "source_session_id": "sess-abc",
    }
    without = {
        "id": "x-0007", "pr_number": 6,
        "pr_url": "https://github.com/owner/repo/pull/6",
        "cwd": str(repo_dir),
    }
    got = {c.node_id: c.source_session_id for c in discover_open_prs([with_sess, without])}
    assert got == {"x-0006": "sess-abc", "x-0007": None}


def test_boundary_node_with_superseded_by_excluded():
    """Boundary: node with superseded_by is excluded (not open)."""
    node = {
        "id": "x-0004",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "superseded_by": "x-0005",
        "cwd": "/tmp/repo",
    }
    result = discover_open_prs([node])
    assert result == []


# ---------------------------------------------------------------------------
# Boundary: max-age exceeded -> "park"
# ---------------------------------------------------------------------------

def test_boundary_max_age_exceeded_parks():
    """Boundary: PR older than max_age_days -> 'park', reason 'max-age'."""
    obs = _obs(state="OPEN", opened_at="2026-05-01T00:00:00Z")
    # NOW is 2026-06-14; difference > 14 days
    d = decide(obs, watermark={}, reviewers=[], merge_ready=False, now_iso=NOW, max_age_days=14)
    assert d.kind == "park"
    assert d.reason == "max-age"


def test_boundary_within_max_age_not_parked():
    """Boundary: PR within max_age_days is not parked by age."""
    obs = _obs(state="OPEN", opened_at="2026-06-10T00:00:00Z")
    d = decide(obs, watermark={}, reviewers=[], merge_ready=False, now_iso=NOW, max_age_days=14)
    assert d.kind == "noop"


# ---------------------------------------------------------------------------
# Boundary: zero reviewers
# ---------------------------------------------------------------------------

def test_boundary_zero_reviewers_new_activity_noop():
    """Boundary: zero configured reviewers + new activity -> 'noop' (review not fired)."""
    obs = _obs(latest_review_ts="2026-06-14T10:00:00Z")
    wm = _watermark(last_review_ts="2026-06-13T00:00:00Z")
    d = decide(obs, watermark=wm, reviewers=[], merge_ready=False, now_iso=NOW)
    assert d.kind == "noop"


def test_boundary_zero_reviewers_merged_fires_merge():
    """Boundary: zero reviewers + MERGED + merge_ready -> 'merge'."""
    obs = _obs(state="MERGED", merged=True)
    d = decide(obs, watermark={}, reviewers=[], merge_ready=True, now_iso=NOW)
    assert d.kind == "merge"


# ---------------------------------------------------------------------------
# merge-not-ready: MERGED but merge_ready False
# ---------------------------------------------------------------------------

def test_merge_not_ready_is_noop():
    """merge-not-ready: state MERGED but merge_ready False -> 'noop', reason 'merge-not-ready'."""
    obs = _obs(state="MERGED", merged=True)
    d = decide(obs, watermark={}, reviewers=[], merge_ready=False, now_iso=NOW)
    assert d.kind == "noop"
    assert d.reason == "merge-not-ready"


# ---------------------------------------------------------------------------
# AC1-ERR: read_pr_state raises ReconcileError on gh failure
# ---------------------------------------------------------------------------

def _make_candidate(pr_number: int = 42, repo_slug: str = "owner/repo") -> PrCandidate:
    return PrCandidate(
        node_id="x-0001",
        pr_number=pr_number,
        pr_url=f"https://github.com/{repo_slug}/pull/{pr_number}",
        repo_dir=None,
        repo_slug=repo_slug,
    )


def _stub_runner_fail(*args, **kwargs) -> subprocess.CompletedProcess:
    """Stub runner that returns rc=1 (gh failure)."""
    return subprocess.CompletedProcess(
        args=args[0],
        returncode=1,
        stdout="",
        stderr="authentication error",
    )


def _stub_runner_ok_merge_state(*args, **kwargs) -> subprocess.CompletedProcess:
    """Stub runner returning a valid OPEN pr state for the merge-state call."""
    payload = json.dumps({
        "number": 42,
        "state": "OPEN",
        "url": "https://github.com/owner/repo/pull/42",
        "mergedAt": None,
    })
    return subprocess.CompletedProcess(args=args[0], returncode=0, stdout=payload, stderr="")


def _stub_runner_ok_with_reviews(*args, **kwargs) -> subprocess.CompletedProcess:
    """Stub runner that handles both the pr-view (merge-state) and reviews calls."""
    cmd = args[0] if args else kwargs.get("args", [])
    # If this is the review/comments call (gh pr view with reviews JSON or gh api)
    if isinstance(cmd, list) and "reviews" in " ".join(cmd):
        payload = json.dumps({
            "number": 42,
            "state": "OPEN",
            "url": "https://github.com/owner/repo/pull/42",
            "mergedAt": None,
            "createdAt": "2026-06-01T00:00:00Z",
            "reviews": [
                {
                    "author": {"login": "codex"},
                    "submittedAt": "2026-06-14T10:00:00Z",
                    "state": "APPROVED",
                }
            ],
        })
        return subprocess.CompletedProcess(args=cmd, returncode=0, stdout=payload, stderr="")
    # Default: merge-state query
    payload = json.dumps({
        "number": 42,
        "state": "OPEN",
        "url": "https://github.com/owner/repo/pull/42",
        "mergedAt": None,
        "createdAt": "2026-06-01T00:00:00Z",
    })
    return subprocess.CompletedProcess(args=cmd, returncode=0, stdout=payload, stderr="")


def test_ac1_err_read_pr_state_raises_reconcile_error_on_gh_failure():
    """AC1-ERR: read_pr_state with stubbed runner returning rc!=0 raises ReconcileError."""
    cand = _make_candidate()
    with pytest.raises(ReconcileError):
        read_pr_state(cand, reviewers=["codex"], runner=_stub_runner_fail)


def test_ac1_err_passing_candidate_returns_observation():
    """AC1-ERR: a sibling candidate with passing runner produces a valid PrObservation."""
    cand = _make_candidate()
    obs = read_pr_state(cand, reviewers=["codex"], runner=_stub_runner_ok_with_reviews)
    assert isinstance(obs, PrObservation)
    assert obs.pr_number == 42
    assert obs.state in {"OPEN", "CLOSED", "MERGED", "UNKNOWN"}


# ---------------------------------------------------------------------------
# [bot] matching: reviewer substring match, case-insensitive, strip [bot]
# ---------------------------------------------------------------------------

def _stub_runner_with_bot_review(login: str, ts: str) -> subprocess.CompletedProcess:
    """Return a runner stub that yields a review authored by `login` at `ts`."""
    payload = json.dumps({
        "number": 42,
        "state": "OPEN",
        "url": "https://github.com/owner/repo/pull/42",
        "mergedAt": None,
        "createdAt": "2026-06-01T00:00:00Z",
        "reviews": [
            {
                "author": {"login": login},
                "submittedAt": ts,
                "state": "COMMENTED",
            }
        ],
    })
    return subprocess.CompletedProcess(args=[], returncode=0, stdout=payload, stderr="")


def test_bot_login_matched_by_substring(monkeypatch):
    """[bot] matching: 'codex' reviewer matches 'chatgpt-codex-connector[bot]' login."""
    # We need to test the bot-matching logic inside read_pr_state indirectly
    # by verifying the latest_review_ts is set correctly.
    cand = _make_candidate()

    def stubbed_runner(*args, **kwargs):
        return _stub_runner_with_bot_review("chatgpt-codex-connector[bot]", "2026-06-14T09:00:00Z")

    obs = read_pr_state(cand, reviewers=["codex"], runner=stubbed_runner)
    assert obs.latest_review_ts == "2026-06-14T09:00:00Z"


def test_non_reviewer_login_does_not_advance_latest_review_ts(monkeypatch):
    """[bot] matching: a non-reviewer login's activity does NOT advance latest_review_ts."""
    cand = _make_candidate()

    def stubbed_runner(*args, **kwargs):
        return _stub_runner_with_bot_review("random-user", "2026-06-14T09:00:00Z")

    obs = read_pr_state(cand, reviewers=["codex"], runner=stubbed_runner)
    assert obs.latest_review_ts is None


def test_bot_matching_is_case_insensitive(monkeypatch):
    """[bot] matching: match is case-insensitive (e.g. 'Codex' reviewer vs 'CODEX-bot[bot]')."""
    cand = _make_candidate()

    def stubbed_runner(*args, **kwargs):
        return _stub_runner_with_bot_review("CODEX-BOT[bot]", "2026-06-14T11:00:00Z")

    obs = read_pr_state(cand, reviewers=["codex"], runner=stubbed_runner)
    assert obs.latest_review_ts == "2026-06-14T11:00:00Z"


# ---------------------------------------------------------------------------
# watermark=None last_review_ts fires on any activity
# ---------------------------------------------------------------------------

def test_null_watermark_fires_on_any_activity():
    """None watermark.last_review_ts means anything newer fires review."""
    obs = _obs(latest_review_ts="2026-06-01T00:00:00Z")
    d = decide(obs, watermark={}, reviewers=["codex"], merge_ready=False, now_iso=NOW)
    assert d.kind == "review"
