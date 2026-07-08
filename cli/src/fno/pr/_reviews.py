"""Optional-review signal for `fno pr status` (x-705b).

x-d996 raised the drain-optional-review floor with SKILL.md prose, but prose is
miss-able: an agent shortcut to `gh pr checks` + `reviewDecision` (empty for a
`COMMENTED` bot review) and promised green without ever reading the inline
findings. This attaches the signal to the ONE command the loop already polls -
`fno pr status` - so the green verdict can't arrive divorced from the
unread-findings state.

The read is strictly additive and time-boxed: any failure degrades to the
`"unknown"` / `None` sentinels and never touches the CI verdict or exit code.
"""
from __future__ import annotations

import json
from typing import Callable, Optional

from fno.graph._reconcile import repo_slug_from_url
from fno.pr._proc import Result, run
from fno.pr_watch._discover import _reviewer_matches

Runner = Callable[..., Result]

# The optional-reviewer bots the x-d996 drain paragraph names. config.review.peers
# (resolved below) extends this; config.review.required_bots is the separate GATE
# (read by loop-check) and is out of scope here.
_OPTIONAL_BOTS = ("gemini-code-assist", "chatgpt-codex-connector")

# Emitted on any review-read failure. A distinct sentinel from an empty list `[]`
# so "read failed" never reads as "nothing posted" (US4).
_UNKNOWN = {"optional_reviews": "unknown", "optional_reviews_unresolved": None}

# One GraphQL page of review threads: each thread's resolved state plus its first
# comment's author (the thread author == the reviewer, used to classify optional).
_THREADS_QUERY = (
    "query($owner:String!,$name:String!,$number:Int!,$cursor:String){"
    "repository(owner:$owner,name:$name){"
    "pullRequest(number:$number){"
    "reviewThreads(first:100,after:$cursor){"
    "pageInfo{hasNextPage endCursor}"
    "nodes{isResolved comments(first:1){nodes{author{login}}}}"
    "}}}}"
)


def _strip_bot(login: str) -> str:
    """Drop a trailing ``[bot]`` for display (GitHub appends it to app logins)."""
    return login[:-5] if login.lower().endswith("[bot]") else login


def optional_reviewer_names(cwd: Optional[str] = None) -> list[str]:
    """The reviewer names that mark a review author as *optional*.

    The single source of truth for the optional set: the hardcoded bots plus
    every `config.review.peers` posting identity (and the shared `peer_identity`).
    A config that can't be read degrades to just the bots - the optional signal
    is advisory, so a missing config never hard-fails.
    """
    names = list(_OPTIONAL_BOTS)
    try:
        from pathlib import Path

        from fno.config import load_settings_for_repo

        review = load_settings_for_repo(Path(cwd) if cwd else Path.cwd()).review
        if review.peer_identity:
            names.append(review.peer_identity)
        for entry in review.peers or []:
            if isinstance(entry, dict):
                names.append(entry.get("identity") or entry.get("provider") or "")
            elif isinstance(entry, str):
                names.append(entry)
    except Exception:  # unreadable/invalid config -> just the hardcoded bots
        pass
    return [n for n in names if n]


def _is_optional(login: str, names: list[str]) -> bool:
    return bool(login) and _reviewer_matches(login, names)


def _fetch_threads(
    pr: str, slug: str, cwd: Optional[str], timeout: float, runner: Runner
) -> "Optional[list[tuple[str, bool]]]":
    """Return [(thread_author_login, is_resolved), ...] or None on any failure."""
    owner, _, name = slug.partition("/")
    if not owner or not name:
        return None
    threads: list[tuple[str, bool]] = []
    cursor: Optional[str] = None
    for _ in range(50):  # bounded (50 * 100 = 5000 threads ceiling)
        args = [
            "api", "graphql",
            "-f", "query=" + _THREADS_QUERY,
            "-f", f"owner={owner}",
            "-f", f"name={name}",
            "-F", f"number={pr}",  # -F coerces to GraphQL Int
        ]
        if cursor:
            args += ["-f", f"cursor={cursor}"]
        res = runner(["gh", *args], cwd=cwd, timeout=timeout)
        if not res.ok or not res.stdout.strip():
            return None
        try:
            data = json.loads(res.stdout)
        except (json.JSONDecodeError, ValueError):
            return None
        # `gh api graphql` exits 0 on a GraphQL-level error (auth/partial): the
        # body carries `errors` and/or a null pullRequest. Treat as unavailable.
        pr_node = (((data.get("data") or {}).get("repository") or {})).get("pullRequest")
        if data.get("errors") or pr_node is None:
            return None
        conn = pr_node.get("reviewThreads") or {}
        for node in conn.get("nodes") or []:
            if not node:
                continue
            cnodes = (node.get("comments") or {}).get("nodes") or []
            author = ""
            if cnodes and cnodes[0]:
                author = (cnodes[0].get("author") or {}).get("login") or ""
            threads.append((author, bool(node.get("isResolved"))))
        page = conn.get("pageInfo") or {}
        if page.get("hasNextPage") and page.get("endCursor"):
            cursor = page["endCursor"]
        else:
            break
    return threads


def read_optional_review_state(
    pr: str,
    cwd: Optional[str] = None,
    *,
    timeout: float = 8.0,
    runner: Runner = run,
) -> dict:
    """Compute {optional_reviews, optional_reviews_unresolved} for PR ``pr``.

    `optional_reviews`: list of `{author, state, inline_count}` for optional
    reviewers who posted, OR `"unknown"` on a read failure. `state` is the
    GitHub review state; `inline_count` is that author's review-thread count (a
    body-only COMMENTED review still lists, with `inline_count: 0`).

    `optional_reviews_unresolved`: count of unresolved (`isResolved == false`)
    threads authored by an optional reviewer - the headline actionable field
    (`green && unresolved == 0` == ready) - OR `None` on a read failure.
    """
    names = optional_reviewer_names(cwd)
    res = runner(["gh", "pr", "view", pr, "--json", "reviews,url"], cwd=cwd, timeout=timeout)
    if not res.ok or not res.stdout.strip():
        return dict(_UNKNOWN)
    try:
        data = json.loads(res.stdout)
    except (json.JSONDecodeError, ValueError):
        return dict(_UNKNOWN)
    slug = repo_slug_from_url(data.get("url") or "")
    if not slug:
        return dict(_UNKNOWN)
    threads = _fetch_threads(pr, slug, cwd, timeout, runner)
    if threads is None:
        return dict(_UNKNOWN)

    by_author: dict[str, dict] = {}

    def _entry(login: str) -> dict:
        key = _strip_bot(login).lower()
        if key not in by_author:
            by_author[key] = {"author": _strip_bot(login), "state": None, "inline_count": 0}
        return by_author[key]

    # Review-level presence + state (covers a body-only COMMENTED review with no
    # thread, which reviewThreads never returns - the Domain Pitfall).
    for review in data.get("reviews") or []:
        login = ((review or {}).get("author") or {}).get("login") or ""
        if not _is_optional(login, names):
            continue
        entry = _entry(login)
        state = review.get("state")
        if state:  # reviews are chronological; last non-empty state wins
            entry["state"] = state

    unresolved = 0
    for author, resolved in threads:
        if not _is_optional(author, names):
            continue
        _entry(author)["inline_count"] += 1
        if not resolved:
            unresolved += 1

    return {
        "optional_reviews": sorted(by_author.values(), key=lambda e: e["author"]),
        "optional_reviews_unresolved": unresolved,
    }
