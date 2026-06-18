"""fno worker external - poll for external review status on a PR.

Migrated from worker/review.py. The polling function is renamed
``external_review`` to distinguish it from the internal orchestrator
entrypoint that lives in ``worker/review.py`` after Phase 06.

When review is pending: exits 0 with {"action": "wait", "next_check_in": 30}
When changes requested: exits 0 with {"action": "llm_review", "comments": [...]}
When approved: writes external artifact, sets external_review_passed: true,
               exits 0 with {"action": "approved"}
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Any, Optional

import yaml

DEFAULT_POLL_INTERVAL = 30  # seconds


def _read_state(state_path: Path) -> dict[str, Any]:
    text = state_path.read_text(encoding="utf-8") if state_path.exists() else ""
    if not text.startswith("---"):
        return {}
    rest = text[3:].lstrip("\n")
    end = rest.find("\n---")
    if end == -1:
        return {}
    return yaml.safe_load(rest[:end]) or {}


def external_review(
    *,
    pr_number: Optional[int],
    state_path: Path,
    artifacts_dir: Optional[Path] = None,
    poll_interval: int = DEFAULT_POLL_INTERVAL,
) -> dict[str, Any]:
    """Poll GitHub for PR review status.

    Args:
        pr_number: PR number to poll. If None, reads from state.
        state_path: Path to target-state.md.
        artifacts_dir: Where to write the external review artifact.
        poll_interval: Seconds to suggest for next check.

    Returns:
        One of:
          {"action": "wait", "next_check_in": N}
          {"action": "llm_review", "comments": [...]}
          {"action": "approved", "external_review_passed": True}
          {"action": "error", "error": str}
    """
    state_path = Path(state_path)
    state = _read_state(state_path)

    if pr_number is None:
        pr_number = state.get("pr_number")

    if pr_number is None:
        return {"action": "error", "error": "no pr_number available"}

    # Fetch PR review state from GitHub
    try:
        result = subprocess.run(
            ["gh", "pr", "view", str(pr_number), "--json",
             "number,state,reviews,reviewRequests,merged,url"],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except FileNotFoundError:
        return {
            "action": "error",
            "error": "gh CLI not installed or not found in PATH",
            "exit_code": 127,
        }
    except subprocess.TimeoutExpired:
        return {
            "action": "error",
            "error": "gh CLI timed out after 30s",
            "exit_code": -1,
        }

    if result.returncode != 0:
        return {"action": "error", "error": result.stderr.strip(), "exit_code": result.returncode}

    try:
        pr_data = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        return {"action": "error", "error": f"invalid JSON from gh: {exc}"}

    reviews = pr_data.get("reviews", [])

    # Check for approved state
    approved = [r for r in reviews if r.get("state") == "APPROVED"]
    changes_requested = [r for r in reviews if r.get("state") == "CHANGES_REQUESTED"]

    if approved and not changes_requested:
        # Write external artifact
        session_id = state.get("session_id", "unknown-session")
        if artifacts_dir is None:
            artifacts_dir = state_path.parent / "artifacts"
        artifacts_dir = Path(artifacts_dir)
        artifacts_dir.mkdir(parents=True, exist_ok=True)
        artifact_path = artifacts_dir / f"external-{session_id}.md"
        artifact_content = (
            f"---\n"
            f"session_id: {session_id}\n"
            f"phase: external\n"
            f"pr_number: {pr_number}\n"
            f"---\n"
            f"# External Review Artifact\n\n"
            f"APPROVED by: {', '.join(r['author']['login'] for r in approved if r.get('author'))}\n"
        )
        # Atomic write: external artifact is factor-2 of the external-review
        # gate. See ship.py for the same rationale.
        from fno.state.io import atomic_write
        atomic_write(artifact_path, artifact_content)
        return {
            "action": "approved",
            "external_review_passed": True,
            "pr_number": pr_number,
            "artifact_path": str(artifact_path),
        }

    if changes_requested:
        comments = [
            {
                "author": r.get("author", {}).get("login", "unknown"),
                "body": r.get("body", ""),
            }
            for r in changes_requested
        ]
        return {
            "action": "llm_review",
            "pr_number": pr_number,
            "comments": comments,
            "next_step": "re-enter after skill dispatch applies fixes",
        }

    # Still pending
    return {
        "action": "wait",
        "pr_number": pr_number,
        "next_check_in": poll_interval,
    }
