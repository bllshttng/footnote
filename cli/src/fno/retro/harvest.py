"""Harvest left-out work for a merged PR.

Three sources, gathered independently so one failing does not sink the others:

1. Carve-out ledger (.fno/carveouts.jsonl) - in-session deferred / oos-bug.
2. Declined reviewer findings - reviewer inline comments minus those a fix commit
   addressed, keyed off the LATEST review state.
3. COMPLETION.md deferred_findings - the done-with-concerns items aggregated at ship.

`gh` unavailable -> the review source yields nothing, a WARN names it, the other
sources still process, and the caller RETAINS the trigger sentinel for retry.
A malformed carveouts.jsonl line is skipped with a warning, never aborting the
whole harvest.
"""
from __future__ import annotations

import json
import re
import subprocess
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Iterable, Optional

from fno.carveout.core import BACKFILL_KIND
from fno.retro.types import (
    KIND_CARVEOUT,
    KIND_DEFERRED,
    KIND_POSTMORTEM,
    KIND_REVIEW,
    RawItem,
)

# Severity badge -> normalized severity (check-pr Step 4 table). No badge -> medium.
_GEMINI_BADGE = re.compile(r"!\[(critical|high|medium|low)\]", re.IGNORECASE)
_CODEX_BADGE = re.compile(r"!\[P([123])\b", re.IGNORECASE)
_CODEX_P_MAP = {"1": "high", "2": "medium", "3": "low"}
DEFAULT_SEVERITY = "medium"

# A gh runner takes argv (without the leading "gh") and returns (rc, stdout, stderr).
GhRunner = Callable[[list[str]], "tuple[int, str, str]"]


def _default_gh_runner(args: list[str]) -> "tuple[int, str, str]":
    try:
        proc = subprocess.run(
            ["gh", *args], capture_output=True, text=True, check=False
        )
        return proc.returncode, proc.stdout, proc.stderr
    except (FileNotFoundError, OSError) as exc:  # gh missing
        return 127, "", str(exc)


def normalize_severity(body: str) -> str:
    """Map a reviewer comment body's badge to critical|high|medium|low.

    No recognizable badge defaults to medium (node tier), per AC2-EDGE.
    """
    m = _GEMINI_BADGE.search(body or "")
    if m:
        return m.group(1).lower()
    m = _CODEX_BADGE.search(body or "")
    if m:
        return _CODEX_P_MAP[m.group(1)]
    return DEFAULT_SEVERITY


def harvest_carveouts(
    repo_root: Path,
    *,
    session_ids: Optional[Iterable[str]] = None,
    source_pr: Optional[int] = None,
    warnings: Optional[list[str]] = None,
) -> list[RawItem]:
    """Read .fno/carveouts.jsonl, optionally filtered to given session ids.

    A malformed line is skipped with a warning (never aborts the harvest).
    """
    warnings = warnings if warnings is not None else []
    ledger = repo_root / ".fno" / "carveouts.jsonl"
    if not ledger.exists():
        return []

    want = set(session_ids) if session_ids is not None else None
    items: list[RawItem] = []
    for lineno, raw in enumerate(
        ledger.read_text(encoding="utf-8").splitlines(), start=1
    ):
        raw = raw.strip()
        if not raw:
            continue
        try:
            rec = json.loads(raw)
        except (json.JSONDecodeError, ValueError):
            warnings.append(f"carveouts.jsonl line {lineno}: malformed JSON, skipped")
            continue
        # A valid-JSON but non-object line (a bare list/string/number) would
        # AttributeError on the rec.get() calls below; skip it like a malformed
        # line rather than crashing the whole harvest (gemini high on PR #465).
        if not isinstance(rec, dict):
            warnings.append(f"carveouts.jsonl line {lineno}: not a JSON object, skipped")
            continue
        if want is not None and rec.get("session_id") not in want:
            continue
        # `backfill` carve-outs are routed to /fno:pr merged's backfill
        # slot, NOT generic retro triage. Skipping them here keeps them out of
        # the classified/landed node set AND out of the harvested-ids the caller
        # consumes, so they SURVIVE in the ledger for post-merge to read and
        # resolve (ab-4a1a4fea, Group 3).
        if rec.get("kind") == BACKFILL_KIND:
            continue
        items.append(
            RawItem(
                kind=KIND_CARVEOUT,
                text=str(rec.get("description", "")),
                source_pr=source_pr,
                source_id=str(rec.get("id", f"carveout:{lineno}")),
                priority=rec.get("priority"),
                title_hint=rec.get("need"),
                subkind=rec.get("kind"),
            )
        )
    return items


def harvest_reviews(
    *,
    comments: list[dict],
    resolved_ids: Optional[Iterable[str]] = None,
    skipped_ids: Optional[Iterable[str]] = None,
    commit_dates: Optional[list[str]] = None,
    author_login: Optional[str] = None,
    source_pr: Optional[int] = None,
    warnings: Optional[list[str]] = None,
) -> list[RawItem]:
    """Turn reviewer inline comments into declined-finding RawItems.

    A comment is a declined candidate unless its id is in ``resolved_ids``
    (a later fix commit / re-review addressed it - the LATEST state wins).
    When ``skipped_ids`` (the author's consolidated "Skipped" table) marks an
    id that ALSO has fix evidence, the fix wins (no candidate) and the
    discrepancy is logged (AC2-FR).

    Additional suppression signals (keyword-only, backward-compatible defaults):

    ``commit_dates`` - ISO8601 commit timestamp strings from the PR. When
    provided, ``addressed_ids_from_comments`` computes which findings were
    addressed via a non-bot in-thread reply plus a later commit (or a
    ``wontfix:`` declaration). Those ids are UNIONED into the resolved set and
    suppressed exactly like thread-resolved findings.

    ``author_login`` - the PR author's GitHub login. When set, any comment
    whose ``reviewer`` field matches (case-insensitive) is skipped entirely.
    This prevents the author's own "Fixed in <sha>" reply comments from being
    re-filed as findings (ab-b4e0061a).

    Each comment dict: {id, body, url?, reviewer?}.
    """
    warnings = warnings if warnings is not None else []
    resolved = {str(i) for i in (resolved_ids or [])}
    skipped = {str(i) for i in (skipped_ids or [])}

    # Union in the signal-based addressed ids when commit_dates are available.
    # This catches the "non-bot reply + fix commit but thread not manually resolved"
    # pattern that addressed_ids_from_threads misses (ab-b4e0061a).
    if commit_dates is not None:
        addressed = addressed_ids_from_comments(comments, commit_dates)
        resolved = resolved | addressed

    author_lower = author_login.lower() if author_login else None

    items: list[RawItem] = []
    for c in comments:
        cid = str(c.get("id", ""))

        # Skip the PR author's own reply comments (e.g. "Fixed in <sha>").
        # GitHub logins are case-insensitive, so compare lowercased.
        reviewer = c.get("reviewer") or ""
        if author_lower and reviewer.lower() == author_lower:
            continue

        # Reply comments (those that answer an inline review thread) are not
        # themselves findings - they are the author's or reviewer's responses
        # to a finding. Harvesting them as independent findings would create
        # duplicate nodes for the same concern (the parent comment is already
        # the finding). Skip any comment that carries an in_reply_to_id.
        if c.get("in_reply_to_id") is not None:
            continue

        if cid and cid in resolved:
            if cid in skipped:
                warnings.append(
                    f"review comment {cid}: author table says skipped but a fix "
                    f"commit addressed it - fix evidence wins (no node)"
                )
            continue  # implemented / resolved -> not surfaced
        body = str(c.get("body", ""))
        items.append(
            RawItem(
                kind=KIND_REVIEW,
                text=body,
                source_pr=source_pr,
                source_id=cid or f"review:{len(items)}",
                severity=normalize_severity(body),
                url=c.get("url"),
                reviewer=reviewer or None,
            )
        )
    return items


def fetch_review_comments(
    pr_number: int,
    *,
    repo: Optional[str] = None,
    gh_runner: GhRunner = _default_gh_runner,
    warnings: Optional[list[str]] = None,
) -> list[dict]:
    """Fetch a PR's inline review comments via `gh api`.

    On any gh error (missing binary, auth, rate limit) returns [] and appends a
    WARN naming the source so the caller can retain the trigger sentinel.
    """
    warnings = warnings if warnings is not None else []
    # With an explicit repo use it; otherwise let gh resolve the current repo
    # via its :owner/:repo placeholders.
    if repo:
        path = f"repos/{repo}/pulls/{pr_number}/comments"
    else:
        path = f"repos/:owner/:repo/pulls/{pr_number}/comments"
    # --slurp is REQUIRED with --paginate: without it gh emits one JSON document
    # per page (concatenated), which json.loads cannot parse. --slurp wraps the
    # pages in a single outer array ([[page1...],[page2...]]).
    args = ["api", path, "--paginate", "--slurp"]
    rc, out, err = gh_runner(args)
    if rc != 0:
        warnings.append(
            f"review harvest: `gh api` failed (rc={rc}); skipping reviewer findings "
            f"for PR #{pr_number} ({err.strip()[:120]})"
        )
        return []
    try:
        raw = json.loads(out) if out.strip() else []
    except (json.JSONDecodeError, ValueError):
        warnings.append("review harvest: could not parse `gh api` output, skipping")
        return []
    # Flatten the slurped array-of-pages into a flat list of comment dicts.
    # Defensive: tolerate a non-slurped flat list (a comment dict per element)
    # so injected/test runners that return a plain array still work.
    flat: list[dict] = []
    if isinstance(raw, list):
        for elem in raw:
            if isinstance(elem, list):
                flat.extend(elem)
            elif isinstance(elem, dict):
                flat.append(elem)
    comments: list[dict] = []
    for c in flat:
        user = c.get("user") or {}
        login = user.get("login") or ""
        # is_bot: True if the GitHub API marks the user as type "Bot" OR the
        # login ends with the "[bot]" suffix (e.g. "gemini[bot]", "codex[bot]").
        # Checking both is defensive: third-party bots sometimes carry type "User"
        # with a "[bot]" login, as codex reviewers commonly do.
        is_bot = (user.get("type") == "Bot") or login.endswith("[bot]")
        # in_reply_to_id: present only on reply comments; convert to str for
        # consistent id comparisons downstream (all ids are strings here).
        raw_reply = c.get("in_reply_to_id")
        in_reply_to_id = str(raw_reply) if raw_reply is not None else None
        comments.append(
            {
                "id": str(c.get("id", "")),
                "body": c.get("body", ""),
                "url": c.get("html_url"),
                "reviewer": login or None,
                "in_reply_to_id": in_reply_to_id,
                "created_at": c.get("created_at"),
                "is_bot": is_bot,
            }
        )
    return comments


# --- commit-based addressed signal -----------------------------------------
#
# Mirror the Rust loop-check signal: a finding is ADDRESSED iff its thread has
# a non-bot in-thread reply AND (a commit landed AFTER the finding's timestamp
# OR a reply body contains the marker "wontfix:"). This catches the "author
# pushes a fix and replies but never clicks Resolve" pattern that thread-state
# alone misses (ab-b4e0061a: "Fixed in <sha>" reply + fix commit -> addressed).


def _parse_ts(s: Optional[str]) -> Optional[datetime]:
    """Parse an ISO8601 timestamp string to a timezone-aware datetime.

    Handles the trailing 'Z' that GitHub uses for UTC (not valid in Python <3.11
    fromisoformat). Returns None on any parse failure so callers can safely skip
    the comparison rather than crashing.
    """
    if not s:
        return None
    try:
        # Replace the trailing Z (UTC designator) with the explicit offset so
        # datetime.fromisoformat accepts it on all supported Python versions.
        normalized = s.replace("Z", "+00:00") if s.endswith("Z") else s
        dt = datetime.fromisoformat(normalized)
        # Ensure the result is timezone-aware for safe cross-comparison.
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        return dt
    except (ValueError, TypeError):
        return None


def addressed_ids_from_comments(
    comments: list[dict],
    commit_dates: list[str],
) -> "set[str]":
    """Compute comment ids that are addressed by a non-bot reply + later commit.

    Pure function, no IO. A TOP-LEVEL comment (no ``in_reply_to_id``) is treated
    as a finding. It is addressed when:

    1. At least one non-bot reply exists in the thread, AND
    2. Either a commit was pushed STRICTLY after the finding's ``created_at``
       timestamp, OR any non-bot reply body contains ``wontfix:`` (any case).

    Comments with missing ``is_bot`` / ``in_reply_to_id`` / ``created_at`` keys
    degrade gracefully (read as not-bot / top-level / no-timestamp) so a caller
    passing enriched-or-legacy comment dicts from either fetch path works without
    transformation.
    """
    # Build a per-parent-id reply map: parent_id -> list of (is_bot, body) tuples.
    reply_map: dict[str, list[tuple[bool, str]]] = {}
    for c in comments:
        parent = c.get("in_reply_to_id")
        if parent is not None:
            is_bot = bool(c.get("is_bot", False))
            body = str(c.get("body") or "")
            reply_map.setdefault(str(parent), []).append((is_bot, body))

    # Pre-parse the commit timestamps once rather than re-parsing every commit
    # for every finding inside the loop (gemini perf, PR #474): the per-finding
    # check below is then a cheap datetime compare against this list.
    parsed_commits = [dt for cd in commit_dates if (dt := _parse_ts(cd)) is not None]

    addressed: set[str] = set()
    for c in comments:
        if c.get("in_reply_to_id") is not None:
            continue  # only evaluate top-level comments as findings
        finding_id = str(c.get("id", ""))
        if not finding_id:
            continue
        replies = reply_map.get(finding_id, [])
        non_bot_replies = [(bot, body) for (bot, body) in replies if not bot]
        if not non_bot_replies:
            continue  # no non-bot reply -> definitely not addressed

        # Check whether any commit landed STRICTLY after the finding was posted.
        # Parse the finding ts once; a missing/unparseable ts biases to
        # not-addressed (we never silently drop a real finding).
        finding_dt = _parse_ts(c.get("created_at"))
        commit_after = (
            any(cd_dt > finding_dt for cd_dt in parsed_commits)
            if finding_dt is not None
            else False
        )
        # Check whether any non-bot reply explicitly waives the finding.
        wontfix = any(
            "wontfix:" in body.lower() for (_, body) in non_bot_replies
        )
        if commit_after or wontfix:
            addressed.add(finding_id)

    return addressed


def fetch_pr_commit_dates(
    pr_number: int,
    *,
    repo: Optional[str] = None,
    gh_runner: GhRunner = _default_gh_runner,
    warnings: Optional[list[str]] = None,
) -> "tuple[list[str], bool]":
    """Fetch commit timestamps for a PR's commits.

    Returns ``(dates, gh_unavailable)`` where ``dates`` is a list of ISO8601
    date strings. Each commit's date is taken from ``commit.committer.date``
    (the wall-clock time the commit was applied to the branch) with a fallback
    to ``commit.author.date`` (written by the author; may predate the push).

    On any gh error (missing binary, auth, rate-limit, unparseable output)
    returns ``([], True)`` and appends a WARN. This is an enrichment signal
    (used to compute addressed findings), not the correctness-bearing resolved
    filter, so a failure does NOT retain the sentinel - the caller treats it
    the same way as the Skipped-table cross-check failure (cosmetic, non-fatal).
    """
    warnings = warnings if warnings is not None else []
    if repo:
        path = f"repos/{repo}/pulls/{pr_number}/commits"
    else:
        path = f"repos/:owner/:repo/pulls/{pr_number}/commits"
    # --slurp wraps paginated pages in a single outer array; without it
    # json.loads fails on a multi-page result (same pattern as fetch_review_comments).
    rc, out, err = gh_runner(["api", path, "--paginate", "--slurp"])
    if rc != 0:
        warnings.append(
            f"review harvest: `gh api` commit-dates failed (rc={rc}); "
            f"skipping addressed-finding enrichment for PR #{pr_number} "
            f"({err.strip()[:120]})"
        )
        return [], True
    try:
        raw = json.loads(out) if out.strip() else []
    except (json.JSONDecodeError, ValueError):
        warnings.append(
            "review harvest: could not parse commit-dates output, skipping enrichment"
        )
        return [], True
    # Flatten the slurped array-of-pages into a flat commit list, exactly as
    # fetch_review_comments does for review comments.
    flat: list[dict] = []
    if isinstance(raw, list):
        for elem in raw:
            if isinstance(elem, list):
                flat.extend(elem)
            elif isinstance(elem, dict):
                flat.append(elem)
    dates: list[str] = []
    for commit in flat:
        c = commit.get("commit") or {}
        committer = c.get("committer") or {}
        author = c.get("author") or {}
        date = committer.get("date") or author.get("date")
        if date:
            dates.append(str(date))
    return dates, False


def fetch_pr_author(
    pr_number: int,
    *,
    repo: Optional[str] = None,
    gh_runner: GhRunner = _default_gh_runner,
    warnings: Optional[list[str]] = None,
) -> Optional[str]:
    """Fetch the PR author's GitHub login.

    Returns the login string on success, or None on any gh error or empty
    response (best-effort; never raises). A gh error appends a WARN; an
    empty-but-successful response (the API returned blank) is silent.

    Used to filter the PR author's own reply comments from the finding set
    (e.g. "Fixed in <sha>" replies should not become backlog nodes).
    """
    warnings = warnings if warnings is not None else []
    if repo:
        path = f"repos/{repo}/pulls/{pr_number}"
    else:
        path = f"repos/:owner/:repo/pulls/{pr_number}"
    rc, out, err = gh_runner(["api", path, "--jq", ".user.login"])
    if rc != 0:
        warnings.append(
            f"review harvest: `gh api` pr-author failed (rc={rc}); "
            f"skipping author-reply filter for PR #{pr_number} "
            f"({err.strip()[:120]})"
        )
        return None
    login = out.strip()
    return login if login else None


# --- resolved-thread state (key off the LATEST review state) ---------------
#
# A reviewer comment alone cannot say whether a finding was implemented: the
# stale comment survives a fix push. The thread's `isResolved` flag is the
# LATEST state - a fix push (or a manual resolve) flips it. We treat every
# comment in a RESOLVED thread as implemented and drop it from candidates,
# which is what fixes "an IMPLEMENTED finding re-filed as a node" (ab-bb7fa74f).

# GraphQL needs an explicit owner/name (no :owner/:repo placeholders), so the
# caller must supply repo as "owner/name" (the sentinel carries it via pr_url).
_REVIEW_THREADS_QUERY = (
    "query($owner:String!,$name:String!,$number:Int!,$cursor:String){"
    "repository(owner:$owner,name:$name){"
    "pullRequest(number:$number){"
    "reviewThreads(first:100,after:$cursor){"
    "pageInfo{hasNextPage endCursor}"
    "nodes{id isResolved isOutdated path comments(first:100){"
    "pageInfo{hasNextPage endCursor}nodes{databaseId}}}"
    "}}}}"
)

# Follow-up query to page a single thread's comments past the first 100, so a
# resolved thread with a long discussion does not drop later databaseIds (which
# would let those comments be harvested as declined - Codex P2 on PR #348).
_THREAD_COMMENTS_QUERY = (
    "query($id:ID!,$cursor:String){"
    "node(id:$id){... on PullRequestReviewThread{"
    "comments(first:100,after:$cursor){"
    "pageInfo{hasNextPage endCursor}nodes{databaseId}}}}}"
)


def _split_repo(repo: Optional[str]) -> "Optional[tuple[str, str]]":
    if repo and "/" in repo:
        owner, _, name = repo.partition("/")
        if owner and name:
            return owner, name
    return None


def fetch_review_thread_state(
    pr_number: int,
    *,
    repo: Optional[str] = None,
    gh_runner: GhRunner = _default_gh_runner,
    warnings: Optional[list[str]] = None,
) -> "tuple[list[dict], bool]":
    """Fetch a PR's review threads with their LATEST ``isResolved`` state.

    Returns ``(threads, gh_unavailable)``. Each thread dict::

        {"is_resolved": bool, "path": str|None, "comment_ids": [str, ...]}

    On any gh error (missing binary, auth, rate limit, unparseable output, a
    GraphQL-level error envelope, or an unresolvable owner/repo) returns
    ``([], True)`` and appends a WARN, so
    the caller RETAINS the trigger sentinel rather than re-filing implemented
    findings under an empty resolved set.
    """
    warnings = warnings if warnings is not None else []
    owner_name = _split_repo(repo)
    if owner_name is None:
        warnings.append(
            f"review harvest: no owner/repo for PR #{pr_number}; cannot read "
            "resolved-thread state, retaining sentinel"
        )
        return [], True
    owner, name = owner_name

    threads: list[dict] = []
    cursor: Optional[str] = None
    for _ in range(50):  # bounded pagination (50 * 100 = 5000 threads ceiling)
        args = [
            "api", "graphql",
            "-f", "query=" + _REVIEW_THREADS_QUERY,
            "-f", f"owner={owner}",
            "-f", f"name={name}",
            "-F", f"number={pr_number}",  # -F coerces to GraphQL Int
        ]
        if cursor:
            args += ["-f", f"cursor={cursor}"]
        rc, out, err = gh_runner(args)
        if rc != 0:
            warnings.append(
                f"review harvest: `gh api graphql` reviewThreads failed (rc={rc}) "
                f"for PR #{pr_number} ({err.strip()[:120]})"
            )
            return [], True
        try:
            data = json.loads(out) if out.strip() else {}
        except (json.JSONDecodeError, ValueError):
            warnings.append(
                "review harvest: could not parse `gh api graphql` reviewThreads output"
            )
            return [], True
        # `gh api graphql` exits 0 for a GraphQL-level error (field auth, query
        # error, partial failure): the body carries a top-level `errors` array
        # and/or a null `pullRequest`. Treating that as "zero resolved threads"
        # would re-file every implemented finding - the exact regression this
        # exists to prevent - so surface it as unavailable and retain.
        pr = (
            ((data.get("data") or {}).get("repository") or {})
        ).get("pullRequest")
        if data.get("errors") or pr is None:
            detail = "GraphQL errors" if data.get("errors") else "no pullRequest in response"
            warnings.append(
                f"review harvest: `gh api graphql` reviewThreads returned {detail} "
                f"for PR #{pr_number}; retaining sentinel"
            )
            return [], True
        conn = pr.get("reviewThreads") or {}
        for node in conn.get("nodes") or []:
            if not node:  # GraphQL can return null nodes on a partial failure
                continue
            comments = node.get("comments") or {}
            cnodes = comments.get("nodes") or []
            cids = [
                str(c["databaseId"])
                for c in cnodes
                if c and c.get("databaseId") is not None
            ]
            # Page past the first 100 comments so a long resolved thread does
            # not drop later databaseIds (Codex P2). On a gh failure mid-page
            # the whole fetch is unavailable -> retain rather than under-report.
            cpage = comments.get("pageInfo") or {}
            if cpage.get("hasNextPage") and node.get("id"):
                more, more_unavailable = _fetch_remaining_thread_comment_ids(
                    node["id"], cpage.get("endCursor"),
                    gh_runner=gh_runner, warnings=warnings,
                )
                if more_unavailable:
                    return [], True
                cids.extend(more)
            threads.append(
                {
                    "is_resolved": bool(node.get("isResolved")),
                    "is_outdated": bool(node.get("isOutdated")),
                    "path": node.get("path"),
                    "comment_ids": cids,
                }
            )
        page = conn.get("pageInfo") or {}
        if page.get("hasNextPage") and page.get("endCursor"):
            cursor = page["endCursor"]
        else:
            break
    return threads, False


def _fetch_remaining_thread_comment_ids(
    thread_id: str,
    after_cursor: Optional[str],
    *,
    gh_runner: GhRunner = _default_gh_runner,
    warnings: Optional[list[str]] = None,
) -> "tuple[list[str], bool]":
    """Page a single review thread's comments past the first 100.

    Returns ``(comment_ids, gh_unavailable)``. Any gh error mid-pagination
    yields ``([], True)`` so the caller treats the resolved-thread read as
    incomplete and retains the sentinel.
    """
    warnings = warnings if warnings is not None else []
    out: list[str] = []
    cursor = after_cursor
    for _ in range(50):  # bounded (50 * 100 = 5000 comments per thread ceiling)
        if not cursor:
            break
        args = [
            "api", "graphql",
            "-f", "query=" + _THREAD_COMMENTS_QUERY,
            "-f", f"id={thread_id}",
            "-f", f"cursor={cursor}",
        ]
        rc, out_str, err = gh_runner(args)
        if rc != 0:
            warnings.append(
                f"review harvest: `gh api graphql` thread-comments page failed "
                f"(rc={rc}) ({err.strip()[:120]}); retaining sentinel"
            )
            return [], True
        try:
            data = json.loads(out_str) if out_str.strip() else {}
        except (json.JSONDecodeError, ValueError):
            warnings.append(
                "review harvest: could not parse thread-comments page output"
            )
            return [], True
        node = (data.get("data") or {}).get("node")
        if data.get("errors") or node is None:
            warnings.append(
                "review harvest: thread-comments page returned errors/null node; "
                "retaining sentinel"
            )
            return [], True
        comments = node.get("comments") or {}
        for c in comments.get("nodes") or []:
            if c and c.get("databaseId") is not None:
                out.append(str(c["databaseId"]))
        cpage = comments.get("pageInfo") or {}
        cursor = cpage.get("endCursor") if cpage.get("hasNextPage") else None
    return out, False


def resolved_ids_from_threads(threads: list[dict]) -> "set[str]":
    """Comment ids in a RESOLVED thread (LATEST state -> finding implemented)."""
    out: set[str] = set()
    for t in threads:
        if t.get("is_resolved"):
            out.update(t.get("comment_ids") or [])
    return out


def addressed_ids_from_threads(threads: list[dict]) -> "set[str]":
    """Comment ids in a thread the LATEST PR state shows as ADDRESSED.

    A thread is addressed when it is RESOLVED (a manual resolve or a fix-push
    auto-resolve) OR OUTDATED (the diff hunk the comment anchored to changed
    after the comment - GitHub's signal that the cited code was edited). Both
    mean the reviewer's specific concern is no longer live on the current head,
    so its comments are dropped from retro candidates rather than re-filed.

    Outdated catches the case the resolved flag alone misses: an author pushes
    a fix for a Gemini/Codex finding WITHOUT clicking "Resolve", so the thread
    stays unresolved but goes outdated (ab-158ab951: 7 already-implemented
    findings re-queued exactly this way). Retro candidates are filed QUEUED
    behind a human `fno backlog pick` ack and the reviewer comment still lives
    on the PR, so biasing toward suppression trades a rare missed follow-up for
    far less spurious-node noise - the node's stated goal. Superset of
    `resolved_ids_from_threads`; back-compatible with thread dicts that predate
    the `is_outdated` field (a missing key reads as not-outdated).
    """
    out: set[str] = set()
    for t in threads:
        if t.get("is_resolved") or t.get("is_outdated"):
            out.update(t.get("comment_ids") or [])
    return out


# --- author "Skipped" reply table (AC2-FR cross-check) ---------------------
#
# `/pr check` posts one consolidated reply comment with a "### Skipped" table:
#   | Reviewer | File | Issue | Reason |
# It carries no comment ids, so we map rows back to threads by file path. The
# mapping powers ONLY the discrepancy warning (author says skipped but the
# thread is resolved -> fix evidence wins); it never files or suppresses a
# node, so a coarse path match is acceptable.
_SKIPPED_HEADER = re.compile(r"^#{2,4}\s+skipped\b", re.IGNORECASE)


def parse_skipped_table(text: str) -> list[dict]:
    """Parse a "### Skipped" markdown table into rows.

    Returns ``[{"reviewer", "file", "issue", "reason"}]`` (file de-backticked).
    Tolerant: no Skipped section / no table -> ``[]``.
    """
    rows: list[dict] = []
    if not text:
        return rows
    in_section = False
    for line in text.splitlines():
        stripped = line.strip()
        if _SKIPPED_HEADER.match(stripped):
            in_section = True
            continue
        if not in_section:
            continue
        if stripped.startswith("#"):
            break  # the next heading closes the section
        if not stripped.startswith("|"):
            continue
        cells = [c.strip() for c in stripped.strip("|").split("|")]
        joined = "".join(cells)
        if not joined or set(joined) <= set("-: "):
            continue  # the |---|---| separator row
        if cells and cells[0].lower() == "reviewer":
            continue  # the header row
        while len(cells) < 4:
            cells.append("")
        rows.append(
            {
                "reviewer": cells[0],
                "file": cells[1].strip("`").strip(),
                "issue": cells[2],
                "reason": cells[3],
            }
        )
    return rows


def fetch_skipped_rows(
    pr_number: int,
    *,
    repo: Optional[str] = None,
    gh_runner: GhRunner = _default_gh_runner,
    warnings: Optional[list[str]] = None,
) -> "tuple[list[dict], bool]":
    """Fetch PR issue comments and parse the author's "### Skipped" reply table.

    Returns ``(rows, gh_unavailable)``. A PR with no skipped table -> ``([],
    False)`` (the common case, NOT an error). A gh error -> ``([], True)`` and
    a WARN; the caller treats a missing cross-check as cosmetic (the resolved
    filter alone is correctness-bearing) and does NOT retain the sentinel for
    it.
    """
    warnings = warnings if warnings is not None else []
    if repo:
        path = f"repos/{repo}/issues/{pr_number}/comments"
    else:
        path = f"repos/:owner/:repo/issues/{pr_number}/comments"
    rc, out, err = gh_runner(["api", path, "--paginate", "--slurp"])
    if rc != 0:
        warnings.append(
            f"review harvest: `gh api` issue-comments failed (rc={rc}) for PR "
            f"#{pr_number}; skipping author Skipped-table cross-check "
            f"({err.strip()[:80]})"
        )
        return [], True
    try:
        raw = json.loads(out) if out.strip() else []
    except (json.JSONDecodeError, ValueError):
        warnings.append("review harvest: could not parse issue-comments output")
        return [], True
    flat: list[dict] = []
    if isinstance(raw, list):
        for elem in raw:
            if isinstance(elem, list):
                flat.extend(elem)
            elif isinstance(elem, dict):
                flat.append(elem)
    rows: list[dict] = []
    for c in flat:
        body = str(c.get("body", ""))
        if any(_SKIPPED_HEADER.match(ln.strip()) for ln in body.splitlines()):
            rows.extend(parse_skipped_table(body))
    return rows, False


def skipped_ids_from_rows(rows: list[dict], threads: list[dict]) -> "set[str]":
    """Map "Skipped" table rows to inline-comment ids by file path (coarse).

    A row marks every comment id on a thread whose path it names (full path or
    basename). Rows with no file, or files matching no thread, contribute
    nothing.
    """
    if not rows or not threads:
        return set()
    by_path: dict = {}
    for t in threads:
        p = (t.get("path") or "").strip()
        if p:
            by_path.setdefault(p, set()).update(t.get("comment_ids") or [])
    out: set[str] = set()
    for row in rows:
        rfile = (row.get("file") or "").strip("`").strip()
        if not rfile:
            continue
        for p, ids in by_path.items():
            # Full-path equality or a basename match on a path boundary; the
            # "/" guard avoids "a.py" spuriously matching "schema.py".
            if p == rfile or p.endswith("/" + rfile) or rfile.endswith("/" + p):
                out.update(ids)
    return out


def harvest_deferred_findings(
    completion_md: Path,
    *,
    source_pr: Optional[int] = None,
    warnings: Optional[list[str]] = None,
) -> list[RawItem]:
    """Parse a COMPLETION.md "## Deferred Findings" section into RawItems.

    Tolerant of a missing file or missing section (returns []). Each bullet
    under the section becomes one deferred-finding item.
    """
    warnings = warnings if warnings is not None else []
    if not completion_md.exists():
        return []
    try:
        text = completion_md.read_text(encoding="utf-8")
    except OSError:
        return []

    items: list[RawItem] = []
    in_section = False
    idx = 0
    for line in text.splitlines():
        if line.strip().lower().startswith("## deferred findings"):
            in_section = True
            continue
        if in_section and line.startswith("## "):
            break  # next top-level section ends the block
        if in_section and line.lstrip().startswith("- "):
            finding = line.lstrip()[2:].strip()
            if finding:
                items.append(
                    RawItem(
                        kind=KIND_DEFERRED,
                        text=finding,
                        source_pr=source_pr,
                        source_id=f"deferred:{idx}",
                    )
                )
                idx += 1
    return items


# ── postmortems (W6 6.2, x-f063) ─────────────────────────────────────────────

# Two on-disk formats coexist: the legacy target-postmortem (YAML frontmatter
# with `blocked_reason: {kind: ...}`) and the finalize.rs artifact (no
# frontmatter; `- termination: **Reason**` bullet). Harvest reads both; the
# consumed_at stamp adds a frontmatter block when none exists.
_PM_TERMINATION_RE = re.compile(r"^-\s*termination:\s*\*{0,2}([A-Za-z_]+)\*{0,2}", re.MULTILINE)
_PM_GIST_LINES = 40
_PM_GIST_CHARS = 4000


def _split_frontmatter(text: str) -> "tuple[Optional[dict], str]":
    """Return (frontmatter dict or None, body). Malformed YAML raises ValueError."""
    if not text.startswith("---"):
        return None, text
    rest = text[3:].lstrip("\n")
    idx = rest.find("\n---")
    if idx == -1:
        return None, text
    import yaml

    try:
        fm = yaml.safe_load(rest[:idx])
    except yaml.YAMLError as exc:
        raise ValueError(f"bad frontmatter: {exc}") from exc
    if not isinstance(fm, dict):
        fm = {}
    return fm, rest[idx + 4 :].lstrip("\n")


def harvest_postmortems(
    postmortems_dir: Path,
    *,
    warnings: Optional[list[str]] = None,
) -> list[RawItem]:
    """Harvest unconsumed postmortem artifacts into RawItems.

    - Skips any file whose frontmatter already carries ``consumed_at`` (AC6-FR).
    - Batches: each item carries only a bounded gist (frontmatter + first-N
      lines), never the whole directory in one blob (Failure Mode boundary).
    - A malformed file is skipped with a warning, never an abort (mirrors the
      carveouts contract).
    """
    warnings = warnings if warnings is not None else []
    if not postmortems_dir.is_dir():
        return []

    items: list[RawItem] = []
    for path in sorted(postmortems_dir.glob("*.md")):
        try:
            text = path.read_text(encoding="utf-8")
            fm, _body = _split_frontmatter(text)
        except (OSError, ValueError) as exc:
            warnings.append(f"postmortem {path.name}: {exc}; skipped")
            continue
        fm = fm or {}
        if fm.get("consumed_at"):
            continue  # already dispositioned by a prior run

        # Termination/blocked reason: legacy frontmatter first, else the
        # finalize.rs bullet line.
        reason = ""
        blocked = fm.get("blocked_reason")
        if isinstance(blocked, dict):
            reason = str(blocked.get("kind") or "")
        if not reason:
            m = _PM_TERMINATION_RE.search(text)
            reason = m.group(1) if m else ""

        gist = "\n".join(text.splitlines()[:_PM_GIST_LINES])[:_PM_GIST_CHARS]
        invocation = str(fm.get("target_invocation") or "").strip()
        hint = f"postmortem {reason or 'unknown'}: {invocation}".strip().rstrip(":")
        items.append(
            RawItem(
                kind=KIND_POSTMORTEM,
                text=gist,
                source_id=f"postmortem:{path.name}",
                title_hint=hint,
                subkind=reason or None,
            )
        )
    return items


def stamp_postmortem_consumed(path: Path, *, ts: Optional[str] = None) -> None:
    """Stamp ``consumed_at`` into a postmortem's frontmatter (idempotent).

    Files without frontmatter (the finalize.rs format) get a minimal block
    prepended; files with one get the key inserted before the closing fence.
    Raises OSError/ValueError to the caller - the disposition already landed,
    so the caller records the failure and the entry is re-deduped next run.
    """
    ts = ts or datetime.now(timezone.utc).isoformat()
    text = path.read_text(encoding="utf-8")
    fm, _body = _split_frontmatter(text)
    if fm is not None and fm.get("consumed_at"):
        return  # already stamped
    if fm is None:
        path.write_text(f"---\nconsumed_at: {ts}\n---\n{text}", encoding="utf-8")
        return
    rest = text[3:].lstrip("\n")
    idx = rest.find("\n---")
    head = text[: len(text) - len(rest) + idx]
    tail = text[len(text) - len(rest) + idx :]
    path.write_text(f"{head}\nconsumed_at: {ts}{tail}", encoding="utf-8")
