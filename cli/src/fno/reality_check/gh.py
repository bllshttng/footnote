"""GitHub PR reality-check implementation.

Calls `gh pr view <N> --json state,mergeable,...` via subprocess and
checks whether the PR's state matches the expected state.

Error semantics:
  - Timeout: {ok: false, error: {kind: "timeout"}} - NEVER silently ok:true
  - PR not found (non-zero exit): {ok: false, error: {kind: "pr_not_found"}}
  - JSON parse error: {ok: false, error: {kind: "parse_error"}}
  - gh not installed: {ok: false, error: {kind: "gh_not_found"}}
  - State mismatch: {ok: false, error: {kind: "state_mismatch", actual, expected}}
  - Success: {ok: true, evidence: {...}}
"""
from __future__ import annotations

import json
import subprocess
from typing import Any, Dict, Optional


_GH_JSON_FIELDS = "state,mergeable,number,title,headRefName,url"


def check_gh(
    *,
    pr_number: Optional[int],
    expect: str = "open",
    timeout: int = 5,
) -> Dict[str, Any]:
    """Check a GitHub PR's current state against an expected value.

    Args:
        pr_number: The PR number to query.
        expect: Expected state string (e.g. "open", "closed", "merged").
        timeout: Subprocess timeout in seconds. Default 5.

    Returns:
        {ok: true, evidence: {...}} on success, or
        {ok: false, error: {kind: ..., ...}} on any failure.
    """
    if pr_number is None:
        return {"ok": False, "error": {"kind": "pr_not_found", "detail": "no pr_number provided"}}

    cmd = ["gh", "pr", "view", str(pr_number), "--json", _GH_JSON_FIELDS]

    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return {"ok": False, "error": {"kind": "timeout", "timeout_seconds": timeout}}
    except FileNotFoundError:
        return {"ok": False, "error": {"kind": "gh_not_found", "detail": "gh CLI not found in PATH"}}

    if result.returncode != 0:
        return {
            "ok": False,
            "error": {
                "kind": "pr_not_found",
                "pr_number": pr_number,
                "stderr": result.stderr.strip(),
            },
        }

    try:
        data = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        return {
            "ok": False,
            "error": {
                "kind": "parse_error",
                "detail": str(exc),
                "raw": result.stdout[:200],
            },
        }

    actual_state = data.get("state", "")
    if actual_state.upper() != expect.upper():
        return {
            "ok": False,
            "error": {
                "kind": "state_mismatch",
                "actual": actual_state,
                "expected": expect,
                "pr_number": pr_number,
            },
        }

    return {"ok": True, "evidence": data}
