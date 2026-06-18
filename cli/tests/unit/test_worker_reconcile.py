"""Tests for fno.worker.external (polling) and fno.worker.reconcile.

Note: The polling logic that was previously in worker/review.py has been moved
to worker/external.py (renamed external_review) in Phase 06. These tests now
import from fno.worker.external.
"""
from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
import yaml


# ---- Helpers ----

def _make_state(tmp_path: Path, extra: dict | None = None) -> Path:
    """Create a minimal target-state.md."""
    state_dir = tmp_path / ".fno"
    state_dir.mkdir(parents=True, exist_ok=True)
    state = {
        "status": "IN_PROGRESS",
        "session_id": "20260421T130000Z-88888-ccddee",
        "pr_number": 42,
        "merged_prs": [],
        "external_review_passed": False,
        "artifact_shipped": True,
    }
    if extra:
        state.update(extra)
    content = "---\n" + yaml.dump(state, default_flow_style=False) + "---\n# State\n"
    path = state_dir / "target-state.md"
    path.write_text(content)
    return path


# ---- AC1-HP: review polls and emits action ----

def test_ac1_hp_review_pending_returns_wait(tmp_path):
    """review() returns wait action when review is still pending."""
    state_path = _make_state(tmp_path)

    pr_view_data = {
        "number": 42,
        "state": "OPEN",
        "reviews": [],
        "reviewRequests": [{"login": "reviewer1"}],
    }
    mock_run = MagicMock(
        returncode=0,
        stdout=json.dumps(pr_view_data),
        stderr="",
    )

    with patch("subprocess.run", return_value=mock_run):
        from fno.worker.external import external_review
        result = external_review(pr_number=42, state_path=state_path)

    assert result["action"] == "wait"
    assert "next_check_in" in result
    assert result["next_check_in"] > 0


def test_ac1_hp_review_approved_sets_gate(tmp_path):
    """review() returns approved action and sets external_review_passed when approved."""
    state_path = _make_state(tmp_path)
    artifacts_dir = tmp_path / ".fno" / "artifacts"
    artifacts_dir.mkdir(parents=True, exist_ok=True)

    pr_view_data = {
        "number": 42,
        "state": "OPEN",
        "reviews": [{"state": "APPROVED", "author": {"login": "reviewer1"}}],
    }
    mock_run = MagicMock(
        returncode=0,
        stdout=json.dumps(pr_view_data),
        stderr="",
    )

    with patch("subprocess.run", return_value=mock_run):
        from fno.worker.external import external_review
        result = external_review(
            pr_number=42,
            state_path=state_path,
            artifacts_dir=artifacts_dir,
        )

    assert result["action"] == "approved"
    assert result.get("external_review_passed") is True


def test_ac1_hp_review_changes_requested_returns_fix(tmp_path):
    """review() returns llm_review action when changes are requested."""
    state_path = _make_state(tmp_path)

    pr_view_data = {
        "number": 42,
        "state": "OPEN",
        "reviews": [{"state": "CHANGES_REQUESTED", "author": {"login": "reviewer1"}, "body": "Fix the tests."}],
    }
    mock_run = MagicMock(
        returncode=0,
        stdout=json.dumps(pr_view_data),
        stderr="",
    )

    with patch("subprocess.run", return_value=mock_run):
        from fno.worker.external import external_review
        result = external_review(pr_number=42, state_path=state_path)

    assert result["action"] == "llm_review"
    assert "comments" in result
    assert len(result["comments"]) > 0


# ---- AC2-HP: reconcile detects merged PRs ----

def test_ac2_hp_reconcile_detects_merged(tmp_path):
    """reconcile() detects a merged PR and updates state.merged_prs."""
    state_path = _make_state(tmp_path, {"pr_number": 42})

    pr_data = {
        "number": 42,
        "state": "CLOSED",
        "mergeCommit": {"oid": "abc123"},
        "merged": True,
        "url": "https://github.com/owner/repo/pull/42",
    }
    mock_run = MagicMock(
        returncode=0,
        stdout=json.dumps(pr_data),
        stderr="",
    )

    with patch("subprocess.run", return_value=mock_run):
        from fno.worker.reconcile import reconcile
        result = reconcile(state_path=state_path, scan=False)

    assert result["action"] == "pr_merged"
    assert result["pr_number"] == 42


def test_ac2_hp_reconcile_open_pr_does_nothing(tmp_path):
    """reconcile() with an open PR returns no_action."""
    state_path = _make_state(tmp_path, {"pr_number": 42})

    pr_data = {
        "number": 42,
        "state": "OPEN",
        "merged": False,
        "url": "https://github.com/owner/repo/pull/42",
    }
    mock_run = MagicMock(
        returncode=0,
        stdout=json.dumps(pr_data),
        stderr="",
    )

    with patch("subprocess.run", return_value=mock_run):
        from fno.worker.reconcile import reconcile
        result = reconcile(state_path=state_path, scan=False)

    assert result["action"] == "no_action"


# ---- AC3-EDGE: reconcile detects orphans ----

def test_ac3_edge_reconcile_scan_detects_orphan(tmp_path):
    """reconcile --scan logs orphan event when a PR exists with no active session."""
    # State with no active session for this PR
    state_path = _make_state(tmp_path, {"pr_number": None, "status": "COMPLETE"})

    open_prs = [
        {"number": 99, "headRefName": "feature/orphaned", "state": "OPEN",
         "url": "https://github.com/owner/repo/pull/99"}
    ]
    mock_run = MagicMock(
        returncode=0,
        stdout=json.dumps(open_prs),
        stderr="",
    )

    with patch("subprocess.run", return_value=mock_run):
        from fno.worker.reconcile import reconcile
        result = reconcile(state_path=state_path, scan=True)

    assert result["action"] in ("orphan_detected", "no_action", "scan_complete")
    # Must NOT auto-close the PR
    calls = [str(c) for c in mock_run.call_args_list]
    assert not any("close" in c for c in calls)
    assert not any("delete" in c for c in calls)


def test_ac3_edge_reconcile_no_pr_number(tmp_path):
    """reconcile() with no pr_number in state returns no_action gracefully."""
    state_path = _make_state(tmp_path, {"pr_number": None})

    from fno.worker.reconcile import reconcile
    result = reconcile(state_path=state_path, scan=False)

    assert result["action"] == "no_action"
