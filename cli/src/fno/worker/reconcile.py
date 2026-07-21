"""fno worker reconcile - detect merged/orphaned/closed PRs.

Updates state + graph atomically. Does NOT auto-close orphaned PRs.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Any

import yaml


def _read_state(state_path: Path) -> dict[str, Any]:
    text = state_path.read_text(encoding="utf-8") if state_path.exists() else ""
    if not text.startswith("---"):
        return {}
    rest = text[3:].lstrip("\n")
    end = rest.find("\n---")
    if end == -1:
        return {}
    return yaml.safe_load(rest[:end]) or {}


def reconcile(
    *,
    state_path: Path,
    scan: bool = False,
) -> dict[str, Any]:
    """Detect merged/orphaned/closed PRs and update state atomically.

    Args:
        state_path: Path to target-state.md.
        scan: If True, scan for orphaned open PRs with no active session.

    Returns:
        One of:
          {"action": "pr_merged", "pr_number": N}
          {"action": "no_action"}
          {"action": "orphan_detected", "orphans": [...]}
          {"action": "scan_complete", "orphans": [...]}
          {"action": "error", "error": str}
    """
    state_path = Path(state_path)
    state = _read_state(state_path)

    if scan:
        return _scan_for_orphans(state)

    pr_number = state.get("pr_number")
    if not pr_number:
        return {"action": "no_action"}

    # Fetch PR status from GitHub
    result = subprocess.run(
        ["gh", "pr", "view", str(pr_number), "--json",
         "number,state,merged,mergeCommit,url"],
        capture_output=True,
        text=True,
    )

    if result.returncode != 0:
        return {"action": "error", "error": result.stderr.strip()}

    try:
        pr_data = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        return {"action": "error", "error": f"invalid JSON from gh: {exc}"}

    if pr_data.get("merged") or pr_data.get("mergeCommit"):
        pr_url = pr_data.get("url", f"https://github.com/pull/{pr_number}")
        return {
            "action": "pr_merged",
            "pr_number": pr_number,
            "pr_url": pr_url,
            "merge_commit": (pr_data.get("mergeCommit") or {}).get("oid"),
        }

    # PR still open or closed-unmerged
    if pr_data.get("state") == "CLOSED" and not pr_data.get("merged"):
        return {
            "action": "pr_closed_unmerged",
            "pr_number": pr_number,
        }

    return {"action": "no_action"}


def _scan_for_orphans(state: dict[str, Any]) -> dict[str, Any]:
    """Scan for open PRs that have no active session in state."""
    result = subprocess.run(
        ["gh", "pr", "list", "--state", "open", "--json",
         "number,headRefName,state,url"],
        capture_output=True,
        text=True,
    )

    if result.returncode != 0:
        return {"action": "error", "error": result.stderr.strip()}

    try:
        open_prs = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        return {"action": "error", "error": f"invalid JSON: {exc}"}

    if not open_prs:
        return {"action": "scan_complete", "orphans": []}

    # A PR is orphaned if the session that created it is not actively IN_PROGRESS
    current_status = state.get("status", "")
    current_pr = state.get("pr_number")

    orphans = []
    for pr in open_prs:
        pr_num = pr.get("number")
        # Not the current active PR - might be orphaned
        if pr_num != current_pr or current_status not in ("IN_PROGRESS", "LOOPING"):
            orphans.append({
                "pr_number": pr_num,
                "branch": pr.get("headRefName"),
                "url": pr.get("url"),
                "note": "open PR with no active fno session",
            })

    if orphans:
        # Log orphan event - do NOT auto-close
        return {
            "action": "orphan_detected",
            "orphans": orphans,
            "note": "orphan resolution is a manual decision - PRs not auto-closed",
        }

    return {"action": "scan_complete", "orphans": []}
