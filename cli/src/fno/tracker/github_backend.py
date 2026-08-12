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

from .types import NodeNotFound, TrackerError, TrackerNode, TrackerState

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
        proc = subprocess.run(
            ["gh", *args], capture_output=True, text=True, timeout=30
        )
        return proc.returncode, proc.stdout, proc.stderr

    def read(self, id: str) -> TrackerNode:
        owner, repo, number = parse_github_id(id)
        rc, out, err = self._gh(
            ["issue", "view", str(number), "-R", f"{owner}/{repo}",
             "--json", "title,state"]
        )
        if rc != 0:
            low = err.lower()
            if any(frag in low for frag in _NOT_FOUND_FRAGMENTS):
                raise NodeNotFound(id)
            raise TrackerError(f"gh issue view failed for {id}: {err.strip()}")
        data = json.loads(out)
        return TrackerNode(
            id=id,
            title=data.get("title"),
            state=_state_from_gh(data.get("state")),
            parent=None,
            blocked_by=[],
        )

    def list_open(self) -> list[TrackerNode]:
        if not self._default_repo:
            # No repo scope configured: cannot enumerate. Callers fall back to
            # the default backend's list_open for dispatch selection.
            return []
        rc, out, err = self._gh(
            ["issue", "list", "-R", self._default_repo, "--state", "open",
             "--json", "number,title,state", "--limit", "100"]
        )
        if rc != 0:
            raise TrackerError(
                f"gh issue list failed for {self._default_repo}: {err.strip()}"
            )
        items = json.loads(out or "[]")
        prefix = self._default_repo
        return [
            TrackerNode(
                id=f"{prefix}#{it.get('number')}",
                title=it.get("title"),
                state=_state_from_gh(it.get("state")),
                parent=None,
                blocked_by=[],
            )
            for it in items
        ]

    def close(self, id: str) -> None:
        owner, repo, number = parse_github_id(id)
        rc, _out, err = self._gh(
            ["issue", "close", str(number), "-R", f"{owner}/{repo}"]
        )
        if rc != 0:
            low = err.lower()
            if any(frag in low for frag in _NOT_FOUND_FRAGMENTS):
                raise NodeNotFound(id)
            raise TrackerError(f"gh issue close failed for {id}: {err.strip()}")
