"""Open-PR discovery and per-PR gh state reading for the PR-state watcher.

Reuses ``fno.graph._reconcile`` primitives for node traversal and gh queries
so the discovery and state logic stays close to its existing sibling
(``scan_merge_drift``) without duplicating it.

The public surface is two pure dataclasses plus two functions:
- ``discover_open_prs`` filters graph entries to candidates worth polling.
- ``read_pr_state`` queries gh for the canonical state of one candidate.

Both are designed for easy testing: discovery accepts an ``entries`` list
rather than reading the graph directly, and ``read_pr_state`` accepts an
injectable ``runner`` so tests can stub gh without a live network call.
"""
from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Optional

from fno.graph._reconcile import (
    PrMergeState,
    PrStateLiteral,
    ReconcileError,
    node_pr_refs,
    query_pr_merge_state,
    repo_slug_from_url,
)


@dataclass(frozen=True)
class PrCandidate:
    """A single open backlog node + PR combination that the watcher should poll.

    Built by ``discover_open_prs``. The ``repo_dir`` and ``repo_slug`` may be
    None when the node's cwd is unresolvable or the PR URL is unparseable;
    those candidates are still returned so the caller can log a skip reason
    rather than silently dropping them.
    """

    node_id: str
    pr_number: int
    pr_url: Optional[str]
    # Path to the local git checkout. None when the node has no cwd or the
    # cwd does not point to an existing directory with a .git entry.
    repo_dir: Optional[Path]
    # ``owner/repo`` parsed from pr_url via repo_slug_from_url. None when the
    # URL is absent or does not match the expected GitHub URL pattern.
    repo_slug: Optional[str]
    # The node's originating session id (warm-route target for the post-merge
    # ritual). None when the node predates provenance stamping.
    source_session_id: Optional[str] = None
    # The originating session's harness (claude|codex|gemini): selects which
    # live vehicle the warm route uses. None -> claude (back-compat default).
    source_harness: Optional[str] = None
    # The originating session's cwd: the direct-finalize rung (x-88df) resolves
    # its on-disk transcript + manifest from here. None -> probe skipped (cold).
    source_cwd: Optional[str] = None


@dataclass(frozen=True)
class PrObservation:
    """Everything the watcher needs to make a dispatch decision for one PR.

    Produced by ``read_pr_state`` from a combination of ``gh pr view`` and
    the review/comment listing. The ``latest_review_ts`` carries the newest
    ISO-8601 timestamp among activity authored by a *configured reviewer*
    (matched case-insensitively, stripping a trailing ``[bot]`` suffix), so
    the ``decide()`` function can compare it to the stored watermark without
    re-doing the matching logic.

    ``state`` is the authoritative merge state; ``decide()`` reads
    ``obs.state == "MERGED"`` directly.  There is no ``merged`` bool field
    -- it was a constructible inconsistency (state="OPEN", merged=True).
    """

    pr_number: int
    state: PrStateLiteral
    # ISO-8601 string of the newest review/comment from a configured reviewer.
    # None when there are no matching reviewers or no reviews at all.
    latest_review_ts: Optional[str]
    # ISO-8601 creation timestamp of the PR itself. Used for the max-age gate.
    opened_at: Optional[str]
    # mergeCommit.oid when the PR is merged -- the shared dedup key the
    # post-merge dispatcher uses, so the daemon and reconcile mark the SAME
    # merge. None on an open PR or when gh omits it.
    merge_sha: Optional[str] = None


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------

def _resolve_repo_dir(node: dict) -> Optional[Path]:
    """Resolve a node's cwd to an existing local git checkout, or None.

    Prefers ``_resolved_cwd`` (set by fno's own resolver) then falls back to
    ``cwd``. A cwd that does not exist on disk or lacks a ``.git`` entry is
    treated as unresolvable: the caller will skip the candidate.
    """
    raw = node.get("_resolved_cwd") or node.get("cwd")
    if not isinstance(raw, str) or not raw:
        return None
    p = Path(raw)
    if not p.is_dir() or not (p / ".git").exists():
        return None
    return p


def _reviewer_matches(login: str, reviewers: list[str]) -> bool:
    """Return True when ``login`` matches any configured reviewer.

    Matching is:
    - Strip a trailing ``[bot]`` suffix from the login (GitHub appends it to
      OAuth-app usernames, e.g. ``chatgpt-codex-connector[bot]``).
    - Do a case-insensitive *substring* check: the configured name appears
      anywhere inside the stripped login.

    This mirrors the ``[bot]`` handling described in ``loopcheck.rs`` in the
    Rust reviewer-matching path.
    """
    stripped = login.lower()
    if stripped.endswith("[bot]"):
        stripped = stripped[:-5]  # remove "[bot]"
    return any(r.lower() in stripped for r in reviewers)


def _max_review_ts(reviews: list[dict], reviewers: list[str]) -> Optional[str]:
    """Return the newest ``submittedAt`` or ``createdAt`` among reviews/comments from configured reviewers.

    Returns None when no reviews match. Lexical ISO-8601 string comparison is
    correct for UTC timestamps (the format is monotone-sortable).
    """
    best: Optional[str] = None
    for review in reviews:
        if not isinstance(review, dict):
            continue
        author = (review.get("author") or {})
        login = author.get("login") or ""
        # Reviews use 'submittedAt'; comments use 'createdAt'
        ts = review.get("submittedAt") or review.get("createdAt") or ""
        if not ts:
            continue
        if not _reviewer_matches(login, reviewers):
            continue
        if best is None or ts > best:
            best = ts
    return best


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------

def _node_watchable(
    node: dict, *, now_iso: Optional[str], max_age_days: int
) -> bool:
    """Whether the watcher should still poll this node's PR(s).

    A node is watchable when it is open, OR it is done-at-PR-green
    (``completed_at`` set, ``superseded_by`` unset) and completed within the
    ``max_age_days`` grace window. This bridges the PR-green -> merge gap:
    ``/target`` finalize stamps ``completed_at`` at PR-green, hours before the
    PR merges, so the merge would otherwise never be detected. A superseded
    node is never watchable; a done node older than the window is dropped.
    """
    if node.get("superseded_by"):
        return False
    completed = node.get("completed_at")
    if not completed:
        return True  # open node
    if not now_iso:
        return False  # cannot age-gate without a clock; preserve old exclusion
    from fno.pr_watch import _days_between

    return _days_between(completed, now_iso) <= max_age_days


def discover_open_prs(
    entries: list[dict],
    *,
    now_iso: Optional[str] = None,
    max_age_days: int = 14,
) -> list[PrCandidate]:
    """Filter graph entries down to PR candidates worth polling.

    Returns one ``PrCandidate`` per (node, pr_number) pair. A node is included
    when it is open, or done-at-PR-green within the ``max_age_days`` grace
    window (see ``_node_watchable``) -- so a PR stays watched across the
    PR-green -> merge window. Superseded nodes and done nodes older than the
    window are excluded; nodes with no PR references are excluded via
    ``node_pr_refs``. An empty graph returns an empty list.

    This function performs NO I/O beyond reading the in-memory ``entries``
    list, making it easy to test in isolation.
    """
    candidates: list[PrCandidate] = []
    for node in entries:
        if not _node_watchable(node, now_iso=now_iso, max_age_days=max_age_days):
            continue
        refs = node_pr_refs(node)
        if not refs:
            continue
        node_id = node.get("id", "")
        repo_dir = _resolve_repo_dir(node)
        for pr_number, pr_url in refs:
            slug = repo_slug_from_url(pr_url)
            candidates.append(
                PrCandidate(
                    node_id=node_id,
                    pr_number=pr_number,
                    pr_url=pr_url,
                    repo_dir=repo_dir,
                    repo_slug=slug,
                    source_session_id=node.get("source_session_id") or None,
                    source_harness=node.get("source_harness") or None,
                    source_cwd=node.get("source_cwd") or None,
                )
            )
    return candidates


def read_pr_state(
    candidate: PrCandidate,
    *,
    reviewers: list[str],
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
    timeout_s: float = 30.0,
) -> PrObservation:
    """Query gh for the current state of a single PR candidate.

    Uses ``query_pr_merge_state`` (injected ``runner``) for the authoritative
    merge state. Additionally reads reviews from ``gh pr view --json
    reviews,createdAt`` to compute ``latest_review_ts``.

    Raises ``ReconcileError`` on any gh failure (non-zero returncode, timeout,
    parse failure, missing binary). The caller is responsible for catching and
    skipping this candidate; the error is NOT swallowed here so the tick loop
    (task 1.2) can record it cleanly.
    """
    repo_slug = candidate.repo_slug
    cwd_str = str(candidate.repo_dir) if candidate.repo_dir else None

    # Step 1: Get merge state via the existing reconcile helper.
    merge_state: PrMergeState = query_pr_merge_state(
        candidate.pr_number,
        repo=repo_slug,
        cwd=cwd_str,
        runner=runner,
        timeout_s=timeout_s,
    )

    # Step 2: Get reviews + comments + PR metadata for latest_review_ts and opened_at.
    # We include 'comments' (issue/review comments) in addition to 'reviews' so
    # that bot activity posted as a comment (not a formal review submission) is
    # reflected in latest_review_ts.  Single round-trip; runner is injectable.
    cmd = ["gh", "pr", "view", str(candidate.pr_number)]
    if repo_slug:
        cmd += ["--repo", repo_slug]
    cmd += ["--json", "reviews,comments,createdAt,number,state,url,mergedAt"]

    try:
        result = runner(
            cmd,
            capture_output=True,
            text=True,
            check=False,
            timeout=timeout_s,
            cwd=cwd_str,
        )
    except subprocess.TimeoutExpired as exc:
        raise ReconcileError(
            f"gh pr view #{candidate.pr_number} (reviews) timed out after {timeout_s}s"
        ) from exc
    except OSError as exc:
        raise ReconcileError(f"gh subprocess failed to launch: {exc}") from exc

    if result.returncode != 0:
        raise ReconcileError(
            f"gh pr view #{candidate.pr_number} (reviews) failed "
            f"(rc={result.returncode}): {(result.stderr or '').strip()}"
        )

    try:
        row = json.loads(result.stdout or "{}")
        if not isinstance(row, dict):
            raise ReconcileError("gh reviews stdout was not a JSON object")
    except json.JSONDecodeError as exc:
        raise ReconcileError(f"gh reviews stdout was not JSON: {exc}") from exc

    reviews_raw = row.get("reviews")
    reviews: list[dict] = reviews_raw if isinstance(reviews_raw, list) else []
    comments_raw = row.get("comments")
    comments: list[dict] = comments_raw if isinstance(comments_raw, list) else []
    # Merge reviews and comments into one activity list for timestamp scanning.
    all_activity = reviews + comments
    opened_at: Optional[str] = row.get("createdAt")
    latest_review_ts = _max_review_ts(all_activity, reviewers) if reviewers else None

    return PrObservation(
        pr_number=candidate.pr_number,
        state=merge_state.state,
        latest_review_ts=latest_review_ts,
        opened_at=opened_at,
        merge_sha=merge_state.merge_sha,
    )
