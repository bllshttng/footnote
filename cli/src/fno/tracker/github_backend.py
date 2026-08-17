"""GitHub Issues NodeTracker backend (the first external backend).

id shape: ``owner/repo#N`` (e.g. ``bllshttng/footnote#123``). read and close
carry their own repo in the id, so they need no configuration. list_open needs
a repo scope, supplied as ``default_repo``; without it, list_open returns an
empty list (``backlog next`` falls back to enumerating the default backend).

parent and blocked_by are deliberately degraded: GitHub sub-issues are recent
and task-list parsing is fragile, so the backend returns None / [] for them
rather than guess. ``advance`` ships degraded on this backend, which is
documented in docs/architecture/external-tracker.md.

All gh I/O goes through one method so tests monkeypatch the subprocess seam
rather than the real network.
"""
from __future__ import annotations

import json
import re
import subprocess
import sys

from .types import (
    NodeNotFound,
    TrackerCandidate,
    TrackerError,
    TrackerNode,
    TrackerState,
)

_GH_ID_RE = re.compile(r"^([A-Za-z0-9._-]+)/([A-Za-z0-9._-]+)#([0-9]+)$")
# stderr fragments gh emits when an issue does not exist (vs. a network/auth
# failure, which surfaces as a TrackerError so the caller can degrade).
_NOT_FOUND_FRAGMENTS = ("could not resolve", "not found", "no issue")


def parse_github_id(id_str: str) -> tuple[str, str, int]:
    """Split ``owner/repo#N`` into (owner, repo, number). Raises on a bad shape."""
    m = _GH_ID_RE.match(id_str)
    if not m:
        raise ValueError(
            f"not a GitHub issue id: {id_str!r} (expected owner/repo#N)"
        )
    return m.group(1), m.group(2), int(m.group(3))


def _state_from_gh(raw: str | None) -> TrackerState:
    # gh issue states: OPEN, CLOSED. (A PR merged state never reaches here since
    # this is the issue command, but treat anything non-OPEN as closed anyway.)
    return TrackerState.open if (raw or "").upper() == "OPEN" else TrackerState.closed


class GitHubIssuesTracker:
    name = "github"

    def __init__(self, default_repo: str | None = None) -> None:
        self._default_repo = default_repo

    # All gh I/O funnels here; tests patch this one method.
    def _gh(self, args: list[str]) -> tuple[int, str, str]:
        try:
            proc = subprocess.run(
                ["gh", *args], capture_output=True, text=True, timeout=30
            )
        except FileNotFoundError as exc:
            raise TrackerError(
                "gh binary not found on PATH; install GitHub CLI to use the "
                "github tracker backend"
            ) from exc
        except subprocess.TimeoutExpired as exc:
            raise TrackerError(f"gh timed out: {' '.join(args)}") from exc
        return proc.returncode, proc.stdout, proc.stderr

    @staticmethod
    def _loads(raw: str, label: str) -> dict:
        # gh can return rc=0 with empty or non-JSON stdout (a deprecation
        # notice on stdout, a truncated pipe); treat that as a backend fault,
        # never a JSONDecodeError escaping past the TrackerError contract.
        try:
            return json.loads(raw) if (raw or "").strip() else {}
        except json.JSONDecodeError as exc:
            raise TrackerError(f"gh returned non-JSON for {label}: {exc}") from exc

    def read(self, id: str) -> TrackerNode:
        owner, repo, number = self._parse_id(id)
        rc, out, err = self._gh(
            ["issue", "view", str(number), "-R", f"{owner}/{repo}",
             "--json", "title,state"]
        )
        if rc != 0:
            low = err.lower()
            if any(frag in low for frag in _NOT_FOUND_FRAGMENTS):
                raise NodeNotFound(id)
            raise TrackerError(f"gh issue view failed for {id}: {err.strip()}")
        data = self._loads(out, id)
        return TrackerNode(
            id=id,
            title=data.get("title"),
            state=_state_from_gh(data.get("state")),
            parent=None,
            blocked_by=[],
        )

    def list_open(self) -> list[TrackerCandidate]:
        if not self._default_repo:
            # No repo scope configured. There is no silent fallback to another
            # backend: warn loudly so a user who set only FNO_TRACKER_BACKEND
            # gets a signal that dispatch selection has no scope to enumerate.
            sys.stderr.write(
                "fno tracker: github backend has no FNO_TRACKER_GITHUB_REPO "
                "scope; list_open returns nothing. Set it to enumerate issues.\n"
            )
            return []
        # gh caps a single listing at 1000 rows. A repo with more than 1000
        # open issues is not fully enumerated here, which ships degraded like
        # parent and blocked_by rather than guess at pagination across forks.
        rc, out, err = self._gh(
            ["issue", "list", "-R", self._default_repo, "--state", "open",
             "--json", "number,title,state,createdAt", "--limit", "1000"]
        )
        if rc != 0:
            raise TrackerError(
                f"gh issue list failed for {self._default_repo}: {err.strip()}"
            )
        items = self._loads(out, self._default_repo) if (out or "").strip() else []
        if isinstance(items, dict):
            items = []
        prefix = self._default_repo
        # GitHub Issues carries no priority/rank footnote can read without
        # extra per-issue API calls, so those two use the projection defaults
        # (p2 / unranked) and footnote's ranking falls back to its post-join
        # tiebreakers. createdAt rides the same listing call, so that ordering
        # input is real rather than degraded. Same degradation class as
        # parent/blocked_by.
        return [
            TrackerCandidate(
                id=f"{prefix}#{it.get('number')}",
                title=it.get("title"),
                state=_state_from_gh(it.get("state")),
                parent=None,
                blocked_by=[],
                created_at=it.get("createdAt"),
            )
            for it in items
        ]

    def close(self, id: str) -> None:
        owner, repo, number = self._parse_id(id)
        rc, _out, err = self._gh(
            ["issue", "close", str(number), "-R", f"{owner}/{repo}"]
        )
        if rc != 0:
            low = err.lower()
            if any(frag in low for frag in _NOT_FOUND_FRAGMENTS):
                raise NodeNotFound(id)
            raise TrackerError(f"gh issue close failed for {id}: {err.strip()}")

    @staticmethod
    def _parse_id(id_str: str) -> tuple[str, str, int]:
        # parse_github_id raises ValueError on a malformed id; the NodeTracker
        # contract speaks TrackerError, so convert at this boundary.
        try:
            return parse_github_id(id_str)
        except ValueError as exc:
            raise TrackerError(str(exc)) from exc
