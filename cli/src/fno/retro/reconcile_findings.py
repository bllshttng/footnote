"""Close phantom retro-triage nodes a later commit already addressed.

Retro files a backlog node from a reviewer's inline comment. On an
autonomously-merged, bot-reviewed PR the fixing commit often lands AFTER the
comment (x-632c: the codex comment posted at 10:14, the fix at 10:36) without
anyone resolving the thread or replying, so the node is filed for work that is
already done. ``harvest.addressed_ids_from_*`` now identifies that case; this
module re-runs that detection against each open retro node's SOURCE PR and
reports the ones now addressed, so a sweep can close them mechanically instead
of leaving phantom work in the backlog.

Pure and I/O-injected: :func:`scan_addressed_findings` takes the harvest
fetchers as parameters (defaulting to the real gh-backed ones), so tests drive
it with canned data and never touch the network. The CLI wrapper
(``fno backlog reconcile-findings``) wires the real fetchers and closes via the
existing ``fno backlog done --force`` path.
"""
from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Callable, Optional

from fno.retro import harvest
from fno.retro.dedup import _TRAILER_RE

# The `Source:` line land writes carries the reviewer comment's permalink; the
# `#discussion_r<id>` fragment is the REST comment databaseId the harvest
# addressed-sets are keyed on, and the leading PR URL scopes gh to the right
# repo (a bare PR number collides across the multi-project graph).
_COMMENT_URL_RE = re.compile(
    r"(https?://github\.com/[^\s)]+/pull/\d+)#discussion_r(\d+)"
)


@dataclass
class AddressedFinding:
    """One open retro node whose originating finding is now addressed."""

    node_id: str
    pr_number: int
    comment_id: str
    repo: Optional[str]  # owner/repo, for gh scoping and the close reason
    signal: str  # "resolved/outdated thread" | "commit-after"


def _node_is_open(node: dict) -> bool:
    # Keyed off raw fields (mirrors graph._reconcile.node_is_open) so it holds on
    # a raw entries list that has not been through recompute_statuses.
    return not node.get("completed_at") and not node.get("superseded_by")


def retro_targets(entries: list) -> list:
    """``(node_id, pr_number, comment_id, repo, pr_url)`` for open retro nodes.

    A node qualifies only when it carries BOTH a retro-triage trailer (proof it
    was filed by triage, with a numeric source_pr) AND a reviewer comment
    permalink (the comment id the addressed-sets are keyed on). Postmortem-
    sourced nodes (trailer ``source_pr=None``) and hand-filed nodes are skipped.

    A node anyone has INVESTED in - a linked plan/design (``plan_path``) - is
    skipped too. The commit-after signal cannot tell a fully-addressed finding
    from one whose loud half was fixed while a residual remains (x-3f39/x-cde1:
    a commit landed on PR #555 after their codex comments, but the residual was
    left for a follow-up PR). A designed node is real tracked work; only
    untouched raw phantoms are safe to close on the heuristic.
    """
    from fno.graph._reconcile import repo_slug_from_url

    out: list = []
    for node in entries:
        nid = node.get("id")
        if not isinstance(nid, str) or not _node_is_open(node):
            continue
        if node.get("plan_path"):
            continue  # designed/being-built: real work, never heuristic-close
        details = str(node.get("details") or "")
        trailer = _TRAILER_RE.search(details)
        if not trailer or trailer.group("pr") == "None":
            continue
        url_match = _COMMENT_URL_RE.search(details)
        if not url_match:
            continue
        pr_url, comment_id = url_match.group(1), url_match.group(2)
        # The permalink's own PR number is authoritative (self-consistent with
        # the comment id and repo); the trailer's source_pr is a cross-check.
        pr_number = int(pr_url.rsplit("/pull/", 1)[1])
        repo = repo_slug_from_url(pr_url)
        out.append((nid, pr_number, comment_id, repo, pr_url))
    return out


def scan_addressed_findings(
    entries: list,
    *,
    thread_state_fetcher: Optional[Callable] = None,
    comments_fetcher: Optional[Callable] = None,
    commit_dates_fetcher: Optional[Callable] = None,
    warnings: Optional[list] = None,
) -> list[AddressedFinding]:
    """Open retro nodes whose finding the source PR now shows as addressed.

    Groups nodes by ``(repo, pr)`` so each PR is fetched once, then reuses the
    harvest addressed-detection: a comment in a resolved/outdated thread, or a
    bot finding a later commit addressed. A PR whose thread state can't be read
    is skipped (never closed on uncertainty) with a warning - the same
    fail-closed bias harvest uses.
    """
    thread_state_fetcher = thread_state_fetcher or harvest.fetch_review_thread_state
    comments_fetcher = comments_fetcher or harvest.fetch_review_comments
    commit_dates_fetcher = commit_dates_fetcher or harvest.fetch_pr_commit_dates
    warnings = warnings if warnings is not None else []

    by_pr: dict = {}
    for nid, pr_number, comment_id, repo, _pr_url in retro_targets(entries):
        by_pr.setdefault((repo, pr_number), []).append((nid, comment_id))

    found: list[AddressedFinding] = []
    for (repo, pr_number), items in by_pr.items():
        threads, unavailable = thread_state_fetcher(
            pr_number, repo=repo, warnings=warnings
        )
        if unavailable:
            warnings.append(
                f"reconcile-findings: PR #{pr_number} thread state unavailable; "
                f"skipping {len(items)} node(s) (never close on uncertainty)"
            )
            continue
        thread_addressed = harvest.addressed_ids_from_threads(threads)
        comments = comments_fetcher(pr_number, repo=repo, warnings=warnings)
        commit_dates, _cd_unavail = commit_dates_fetcher(
            pr_number, repo=repo, warnings=warnings
        )
        comment_addressed = harvest.addressed_ids_from_comments(comments, commit_dates)

        for nid, comment_id in items:
            if comment_id in thread_addressed:
                signal = "resolved/outdated thread"
            elif comment_id in comment_addressed:
                signal = "commit-after"
            else:
                continue
            found.append(
                AddressedFinding(
                    node_id=nid,
                    pr_number=pr_number,
                    comment_id=comment_id,
                    repo=repo,
                    signal=signal,
                )
            )
    return found
