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
from pathlib import Path
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
    `config.review.optional_apps` and every `config.review.peers` posting
    identity (and the shared `peer_identity`). A config that can't be read
    degrades to just the bots - the optional signal is advisory, so a missing
    config never hard-fails.
    """
    names = list(_OPTIONAL_BOTS)
    try:
        from pathlib import Path

        from fno.config import load_settings_for_repo

        review = load_settings_for_repo(Path(cwd) if cwd else Path.cwd()).review
        # optional_apps is the config's own honored-if-present optional-bot list;
        # excluding it would hide a configured optional app's findings.
        names.extend(review.optional_apps or [])
        if review.peer_identity:
            names.append(review.peer_identity)
        for entry in review.peers or []:
            if isinstance(entry, dict):
                # Identity-free peers attest locally and are not GitHub review
                # authors. Only the explicit legacy posting carrier belongs in
                # this login-matching set.
                names.append(entry.get("identity") or "")
    except Exception:  # unreadable/invalid config -> just the hardcoded bots
        pass
    return [n for n in names if n]


# x-0eaf: coverage read degrades to this on any failure (additive, fail-open).
_UNKNOWN_COVERAGE = {"coverage": "unknown", "reviewed_count": None}


def _repo_root(cwd: Optional[str] = None) -> Path:
    """Git top-level for ``cwd``, so coverage is found from a subdirectory."""
    base = Path(cwd) if cwd else Path.cwd()
    try:
        import subprocess

        root = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True, text=True, cwd=str(base), timeout=5,
        ).stdout.strip()
        if root:
            base = Path(root)
    except Exception:  # noqa: BLE001 - no git -> the caller's dir is the root
        pass
    return base


def _scan_coverage(
    path: Path, pr_number: int, repo_slug: Optional[str] = None
) -> tuple[Optional[dict], str]:
    """Latest ``review_coverage`` data for ``pr_number`` in one events log.

    Returns ``(data, ts)``; ``(None, "")`` when the log has no match.

    Streams with a substring prefilter rather than using
    ``fno.events.log.read_events``: these logs reach tens of MB (34.7 MB in this
    repo at time of writing), and read_events slurps the whole file AND raises
    on the first malformed line. Since every caller here wraps the read in a
    fail-closed ``except``, one corrupt byte anywhere in the log would wedge the
    merge gate permanently and silently. Skipping a bad line is the honest
    behavior for an append-only log written by several processes.

    ``repo_slug``, when given, must equal the event's ``repo``. Callers pass it
    for the CROSS-PROJECT global log, where ``pr`` is a bare integer and another
    repo's PR of the same number would otherwise satisfy this gate. Events
    predating the ``repo`` field never match a scoped scan, by design.
    """
    latest: Optional[dict] = None
    latest_ts = ""
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            for raw in fh:
                # Cheap reject before the JSON parse; 99.99% of lines are other
                # event types and the logs are large enough for this to matter.
                if "review_coverage" not in raw:
                    continue
                try:
                    ev = json.loads(raw)
                except ValueError:
                    continue
                if not isinstance(ev, dict) or ev.get("type") != "review_coverage":
                    continue
                data = ev.get("data") or {}
                if not isinstance(data, dict):
                    continue
                try:
                    if int(data.get("pr", -1)) != pr_number:
                        continue
                except (TypeError, ValueError):
                    continue
                if repo_slug is not None and data.get("repo") != repo_slug:
                    continue
                latest = data
                latest_ts = str(ev.get("ts") or "")
    except OSError:
        return None, ""
    return latest, latest_ts


def _coverage_logs(
    cwd: Optional[str] = None, project_events: Optional[Path] = None
) -> tuple[Path, Optional[Path], Optional[str]]:
    """The logs a coverage read consults: ``(project, global_or_None, slug)``.

    The global log is None when it cannot be scanned safely - no ``~/.fno``, the
    same file as the project log, or no resolvable git-remote slug. Without a
    slug nothing in the cross-project log can be attributed to this repo, so
    scanning it could only produce a cross-repo false positive.

    Separate from the scan so a caller that only wants to NAME the logs (the
    refusal text) does not read tens of MB of JSONL to learn two filenames.
    """
    root = _repo_root(cwd)
    project_path = (
        project_events if project_events is not None else root / ".fno" / "events.jsonl"
    )
    try:
        from fno import paths as _paths

        # global_events_json(), not state_dir()/"events.jsonl": a RELATIVE
        # config.state_dir resolves into the repo checkout, while the global
        # journal deliberately falls back to ~/.fno - which is also the path the
        # Rust writer hardcodes. Reading state_dir() directly would scan a file
        # nobody writes on exactly those configs, silently restoring the bug.
        global_path = _paths.global_events_json()
        slug = _paths._slug_from_git_remote(root)
    except Exception:  # noqa: BLE001 - no global log -> project log alone
        return project_path, None, None
    if global_path == project_path or not slug:
        return project_path, None, slug
    return project_path, global_path, slug


def coverage_sources(cwd: Optional[str] = None) -> list[str]:
    """The event logs a coverage read would consult, for a refusal message."""
    project_path, global_path, _ = _coverage_logs(cwd)
    return [str(project_path)] + ([str(global_path)] if global_path is not None else [])


def latest_review_coverage(
    pr_number: int,
    cwd: Optional[str] = None,
    project_events: Optional[Path] = None,
) -> Optional[dict]:
    """Latest ``review_coverage`` data for a PR, or None.

    Reads BOTH logs loop-check writes, because which one holds the attestation
    depends on where the reviewing session happened to stand. The stop hook
    writes into the events file of the directory it runs in, so a review
    attested inside a worktree lands in that worktree's project log; a merge run
    from canonical reads canonical's. They agreed only by luck, and the
    disagreement read as "nobody reviewed this" (x-f43c). ``~/.fno`` is the one
    file both stand in, so it is the tiebreaker rather than a fallback.

    The project log is scanned unscoped (it is already repo-local); the global
    log is scoped by git-remote slug. Newest ``ts`` wins across the two, so a
    project-only event from an older binary still beats a stale global one.

    A caller that refuses on a None names those logs with :func:`coverage_sources`:
    a gate that reports a count and not a location is what taught two workers to
    design around a green gate instead of looking at where it read.

    ``project_events`` overrides the derived project log for callers that hold
    an explicit path (the post-merge gate-escape detector, and its tests).
    """
    project_path, global_path, slug = _coverage_logs(cwd, project_events)

    best, best_ts = _scan_coverage(project_path, pr_number)

    if global_path is not None:
        other, other_ts = _scan_coverage(global_path, pr_number, repo_slug=slug)
        if other is not None and (best is None or other_ts > best_ts):
            best, best_ts = other, other_ts

    return best


def read_review_coverage(pr_number: int, cwd: Optional[str] = None) -> dict:
    """The latest ``review_coverage`` verdict for a PR (loop-check emits it every
    gate eval). Additive and fail-open: any failure degrades to the unknown
    sentinel. Python consumes the event rather than recomputing (Ownership: Rust
    computes, Python reads), so a human and the loop see one number for the same
    PR.
    """
    try:
        latest = latest_review_coverage(pr_number, cwd)
    except Exception:  # noqa: BLE001 - additive signal, never hard-fails
        return dict(_UNKNOWN_COVERAGE)
    if latest is None:
        return dict(_UNKNOWN_COVERAGE)
    return {
        "coverage": latest.get("coverage", "unknown"),
        "reviewed_count": latest.get("reviewed_count"),
    }


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
        if not isinstance(data, dict):  # a valid non-object JSON is unavailable
            return None
        # `gh api graphql` exits 0 on a GraphQL-level error (auth/partial): the
        # body carries `errors` and/or a null pullRequest. Treat as unavailable.
        pr_node = (((data.get("data") or {}).get("repository") or {})).get("pullRequest")
        if data.get("errors") or pr_node is None:
            return None
        conn = pr_node.get("reviewThreads") or {}
        for node in conn.get("nodes") or []:
            if not isinstance(node, dict):  # GraphQL can return null/odd nodes
                continue
            cnodes = (node.get("comments") or {}).get("nodes") or []
            author = ""
            if cnodes and isinstance(cnodes[0], dict):
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
    if not isinstance(data, dict):  # a valid non-object JSON degrades to unknown
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
        entry = by_author.get(key)
        if entry is None:
            entry = {"author": _strip_bot(login), "state": None, "inline_count": 0}
            by_author[key] = entry
        return entry

    # Review-level presence + state (covers a body-only COMMENTED review with no
    # thread, which reviewThreads never returns - the Domain Pitfall).
    for review in data.get("reviews") or []:
        if not isinstance(review, dict):
            continue
        login = (review.get("author") or {}).get("login") or ""
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
