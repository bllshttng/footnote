"""Tests for fno.worker.external - polling for external PR review status.

Migrated from the original worker/review.py polling tests.
The function is now ``external_review`` in ``worker.external``.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
import yaml


# ---- Helpers ----

def _make_state(tmp_path: Path, extra: dict | None = None) -> Path:
    """Create a minimal target-state.md."""
    state = {
        "status": "IN_PROGRESS",
        "session_id": "20260421T120000Z-99999-aabbcc",
        "pr_number": 42,
    }
    if extra:
        state.update(extra)
    content = "---\n" + yaml.dump(state, default_flow_style=False) + "---\n# State\n"
    path = tmp_path / "target-state.md"
    path.write_text(content, encoding="utf-8")
    return path


def _gh_pr_json(
    state: str = "OPEN",
    reviews: list | None = None,
) -> str:
    """Return a JSON string as gh pr view would."""
    return json.dumps({
        "number": 42,
        "state": state,
        "merged": False,
        "url": "https://github.com/org/repo/pull/42",
        "reviews": reviews or [],
        "reviewRequests": [],
    })


# ---- AC1-PARITY: polling behavior unchanged after move ----

class TestExternalReviewImport:
    """AC1-PARITY: external_review is importable from fno.worker.external."""

    def test_import_succeeds(self) -> None:
        from fno.worker.external import external_review  # noqa: F401

    def test_function_exists(self) -> None:
        from fno.worker import external as ext_mod
        assert callable(getattr(ext_mod, "external_review", None))


class TestExternalReviewNoPR:
    """AC1-PARITY: returns error when no PR number available."""

    def test_no_pr_number_in_args_or_state(self, tmp_path: Path) -> None:
        from fno.worker.external import external_review

        # State without pr_number
        state_path = tmp_path / "target-state.md"
        state_path.write_text("---\nstatus: IN_PROGRESS\n---\n", encoding="utf-8")

        result = external_review(pr_number=None, state_path=state_path)
        assert result["action"] == "error"
        assert "pr_number" in result["error"]

    def test_no_state_file(self, tmp_path: Path) -> None:
        from fno.worker.external import external_review

        state_path = tmp_path / "missing-state.md"
        result = external_review(pr_number=None, state_path=state_path)
        assert result["action"] == "error"


class TestExternalReviewWait:
    """AC1-PARITY: returns wait when PR is pending review."""

    def test_no_reviews_returns_wait(self, tmp_path: Path) -> None:
        from fno.worker.external import external_review

        state_path = _make_state(tmp_path)
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(
                returncode=0,
                stdout=_gh_pr_json(reviews=[]),
                stderr="",
            )
            result = external_review(pr_number=42, state_path=state_path)

        assert result["action"] == "wait"
        assert result["pr_number"] == 42
        assert "next_check_in" in result

    def test_poll_interval_propagated(self, tmp_path: Path) -> None:
        from fno.worker.external import external_review

        state_path = _make_state(tmp_path)
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(
                returncode=0,
                stdout=_gh_pr_json(reviews=[]),
                stderr="",
            )
            result = external_review(pr_number=42, state_path=state_path, poll_interval=60)

        assert result["next_check_in"] == 60


class TestExternalReviewChangesRequested:
    """AC1-PARITY: returns llm_review when changes are requested."""

    def test_changes_requested(self, tmp_path: Path) -> None:
        from fno.worker.external import external_review

        state_path = _make_state(tmp_path)
        reviews = [
            {"state": "CHANGES_REQUESTED", "author": {"login": "reviewer1"}, "body": "fix this"}
        ]
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(
                returncode=0,
                stdout=_gh_pr_json(reviews=reviews),
                stderr="",
            )
            result = external_review(pr_number=42, state_path=state_path)

        assert result["action"] == "llm_review"
        assert result["pr_number"] == 42
        assert len(result["comments"]) == 1
        assert result["comments"][0]["author"] == "reviewer1"


class TestExternalReviewApproved:
    """AC1-PARITY: writes artifact and returns approved."""

    def test_approved_writes_artifact(self, tmp_path: Path) -> None:
        from fno.worker.external import external_review

        state_path = _make_state(tmp_path)
        artifacts_dir = tmp_path / "artifacts"
        reviews = [
            {"state": "APPROVED", "author": {"login": "approver1"}, "body": "LGTM"}
        ]
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(
                returncode=0,
                stdout=_gh_pr_json(reviews=reviews),
                stderr="",
            )
            result = external_review(
                pr_number=42,
                state_path=state_path,
                artifacts_dir=artifacts_dir,
            )

        assert result["action"] == "approved"
        assert result["external_review_passed"] is True
        assert artifacts_dir.exists()

    def test_approved_with_pending_also_present_returns_approved(self, tmp_path: Path) -> None:
        """If approved reviews exist and no changes_requested, it's approved."""
        from fno.worker.external import external_review

        state_path = _make_state(tmp_path)
        reviews = [
            {"state": "APPROVED", "author": {"login": "a"}, "body": ""},
            {"state": "COMMENTED", "author": {"login": "b"}, "body": "nice"},
        ]
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(
                returncode=0,
                stdout=_gh_pr_json(reviews=reviews),
                stderr="",
            )
            result = external_review(pr_number=42, state_path=state_path, artifacts_dir=tmp_path / "artifacts")

        assert result["action"] == "approved"


class TestExternalReviewGhError:
    """AC1-PARITY: returns error when gh CLI fails."""

    def test_gh_exit_nonzero(self, tmp_path: Path) -> None:
        from fno.worker.external import external_review

        state_path = _make_state(tmp_path)
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(returncode=1, stdout="", stderr="not found")
            result = external_review(pr_number=42, state_path=state_path)

        assert result["action"] == "error"
        assert "exit_code" in result

    def test_gh_invalid_json(self, tmp_path: Path) -> None:
        from fno.worker.external import external_review

        state_path = _make_state(tmp_path)
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(returncode=0, stdout="not json!", stderr="")
            result = external_review(pr_number=42, state_path=state_path)

        assert result["action"] == "error"
        assert "JSON" in result["error"]


# ---- H5: gh not installed -> structured error dict (no crash) ----

class TestH5GhNotInstalled:
    """H5: FileNotFoundError when gh CLI missing -> structured error, not crash."""

    def test_gh_not_installed_returns_error_dict(self, tmp_path: Path) -> None:
        """subprocess.run raises FileNotFoundError -> {'action': 'error', 'exit_code': 127}."""
        from fno.worker.external import external_review

        state_path = _make_state(tmp_path)
        with patch("subprocess.run", side_effect=FileNotFoundError("gh: command not found")):
            result = external_review(pr_number=42, state_path=state_path)

        assert result["action"] == "error"
        assert result["exit_code"] == 127
        assert "gh" in result["error"].lower() or "not installed" in result["error"].lower()

    def test_gh_timeout_returns_error_dict(self, tmp_path: Path) -> None:
        """subprocess.TimeoutExpired -> structured error dict, not crash."""
        from fno.worker.external import external_review

        state_path = _make_state(tmp_path)
        with patch(
            "subprocess.run",
            side_effect=subprocess.TimeoutExpired(cmd=["gh"], timeout=30),
        ):
            result = external_review(pr_number=42, state_path=state_path)

        assert result["action"] == "error"
        # "timed out" or "timeout" both acceptable
        assert "time" in result["error"].lower()

    def test_subprocess_run_has_timeout_kwarg(self, tmp_path: Path) -> None:
        """subprocess.run must be called with timeout= to avoid hanging forever."""
        from fno.worker.external import external_review

        state_path = _make_state(tmp_path)
        with patch("subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(returncode=0, stdout=_gh_pr_json(), stderr="")
            external_review(pr_number=42, state_path=state_path)

        call_kwargs = mock_run.call_args
        # timeout must be passed as a keyword argument
        assert "timeout" in call_kwargs.kwargs, (
            "subprocess.run must be called with timeout= to prevent hanging"
        )
