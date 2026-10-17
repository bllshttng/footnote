"""Optional-review signal for `fno do pr status`.

 raised the drain-optional-review floor with SKILL.md prose, but prose is
miss-able: an agent shortcut to `gh pr checks` + `reviewDecision` (empty for a
`COMMENTED` bot review) and promised green without ever reading the inline
findings. This attaches the signal to the ONE command the loop already polls -
`fno do pr status` - so the green verdict can't arrive divorced from the
unread-findings state.

The read is strictly additive and time-boxed: any failure degrades to the
`"unknown"` / `None` sentinels and never touches the CI verdict or exit code.
"""
from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Callable, Iterator, Optional

from fno.graph._reconcile import repo_slug_from_url
from fno.pr import _quota
from fno.pr._proc import Result, run
from fno.pr_watch._discover import _reviewer_matches

Runner = Callable[..., Result]

#: Freshness labels the Rust producer writes that count toward coverage.
#: The carried_interdiff labels are parameterized by their measured numbers
#: (`carried_interdiff(n=6, cap=100)`), so membership is a prefix test on
#: that one shape rather than an open-ended set the producer could drift from.
_COUNTED_FRESHNESS = frozenset(
    {"fresh", "carried_base_sync", "carried_docs_only", "carried_subset"}
)
_INTERDIFF_PREFIX = "carried_interdiff("


def counted_freshness(label: object) -> bool:
    """Whether a stored freshness label counts toward coverage."""
    if not isinstance(label, str):
        return False
    return label in _COUNTED_FRESHNESS or (
        label.startswith(_INTERDIFF_PREFIX) and label.endswith(")")
    )
_KNOWN_REVIEW_VERDICTS = {"reviewed", "stale", "refused", "errored", "absent"}
_KNOWN_COVERAGE_PRODUCERS = {"github_app", "local_attestation"}
_KNOWN_REVIEW_STATES = {"reviewed", "unreviewed", "reviewer_refused"}

# Emitted on any review-read failure. A distinct sentinel from an empty list `[]`
# so "read failed" never reads as "nothing posted" (US4).
_UNKNOWN = {
    "optional_reviews": "unknown",
    "optional_reviews_unresolved": None,
    "optional_reviews_resolved_unchanged": None,
}

# One GraphQL page of review threads: each thread's resolved state plus its first
# comment's author (the thread author == the reviewer, used to classify optional).
_THREADS_QUERY = (
    "query($owner:String!,$name:String!,$number:Int!,$cursor:String){"
    "repository(owner:$owner,name:$name){"
    "pullRequest(number:$number){"
    "reviewThreads(first:100,after:$cursor){"
    "pageInfo{hasNextPage endCursor}"
    "nodes{isResolved isOutdated comments(first:1){nodes{author{login}}}}"
    "}}}}"
)


def _strip_bot(login: str) -> str:
    """Drop a trailing ``[bot]`` for display (GitHub appends it to app logins)."""
    return login[:-5] if login.lower().endswith("[bot]") else login


def optional_reviewer_names(cwd: Optional[str] = None) -> list[str]:
    """The reviewer names that mark a review author as *optional*.

    The single source of truth for the optional set: the resolved
    `config.review.optional_apps` (which DEFAULTS to the built-in two logins -
    the same default the Rust gate's `resolved_optional_bots` falls back to, so
    `fno do pr status` and the loop gate measure the same lanes on one PR) plus
    every `config.review.peers` posting identity (and the shared
    `peer_identity`). A config that can't be read degrades to that same default
    - the optional signal is advisory, so a missing config never hard-fails.
    """
    from fno.config import DEFAULT_OPTIONAL_APPS

    names: list[str] = []
    try:
        from pathlib import Path

        from fno.config import load_settings_for_repo, resolved_optional_apps

        review = load_settings_for_repo(Path(cwd) if cwd else Path.cwd()).review
        # optional_apps is the config's own honored-if-present optional-bot list
        # (unset -> the built-in default; [] -> an explicit opt-out that wins).
        names.extend(resolved_optional_apps(review))
        if review.peer_identity:
            names.append(review.peer_identity)
        for entry in review.peers or []:
            if isinstance(entry, dict):
                # Identity-free peers attest locally and are not GitHub review
                # authors. Only the explicit legacy posting carrier belongs in
                # this login-matching set.
                names.append(entry.get("identity") or "")
    except Exception:  # unreadable/invalid config -> the built-in default
        names = list(DEFAULT_OPTIONAL_APPS)
    return [n for n in names if n]


# coverage read degrades to this on any failure (additive, fail-open).
# Annotated because the values are heterogeneous (str, None, list) and mypy
# cannot infer a useful type from the literal alone.
_UNKNOWN_COVERAGE: dict[str, object] = {
    "coverage": "unknown",
    "reviewed_count": None,
    "self_attested_count": None,
    "head_sha": None,
    "stale_verdicts": [],
    "note": "coverage probe failed",
}

#: The remedy an unmeasurable coverage answer names: the instrument is
#: missing, and the fix is installing it, never recomputing the answer in a
#: second language (the mirror this constant's path replaces did exactly
#: that, and diverged from the gate).
_UNMEASURABLE_REASON = "fno-agents not on PATH; install it or run fno doctor update"

# A TERMINAL PR (merged or closed) is never ASKED for coverage: the gate guards
# what WOULD merge and a terminal PR has no would left. Distinct from
# _UNKNOWN_COVERAGE on purpose - that one means the instrument was asked and
# failed, and it carries its own `review_coverage_unknown` blocker. Spelling a
# deliberate skip with the instrument-failed sentinel is the absence-vs-outcome
# collapse this gate refuses everywhere else: a reader cannot tell "nobody
# looked because there was nothing to look at" from "the probe died". The
# counts are 0 and the list empty because that IS the answer, not a guess.
_NOT_ASKED_COVERAGE: dict[str, object] = {
    "coverage": "not_asked",
    "reviewed_count": 0,
    "self_attested_count": 0,
    "head_sha": None,
    "stale_verdicts": [],
    "note": "not asked: PR is terminal (merged or closed); this says nothing about coverage at merge time",
}


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


# Repo identity moved down to `fno.paths` (the platform layer) when the mirrored
# attestation in `fno.events.cli` needed the same parser: `fno.events` may not
# import `fno.pr`, and a second copy would be two parsers to hold in parity with
# `finalize.rs` instead of one. Re-exported under the old private names so every
# call site and test here reads unchanged.
from fno.paths import repo_identity as _repo_identity  # noqa: E402


def journal_lines(path: Path, types: tuple[str, ...]) -> Iterator[str]:
    """The lines of ``path``'s journal for ``types``, across rotations.

    The native event read imports the live journal, then answers the store
    rows in commit order. Without the binary the live file alone answers.
    """
    from fno.events.store_client import native_rows

    live = path.resolve()
    live = live.with_name(re.sub(r"\.\d+$", "", live.name))
    rows = native_rows(live, types=list(types), include_rejected=True, legacy_fallback=True)
    if rows is not None:
        for line in rows:
            yield line + "\n"
        return
    try:
        yield from live.read_text(encoding="utf-8", errors="replace").splitlines(keepends=True)
    except OSError:
        return


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
        for raw in journal_lines(path, ("review_coverage",)):
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
) -> tuple[Optional[Path], Optional[Path], Optional[str]]:
    """The logs a coverage read consults: ``(project, global, slug)``, either None.

    The project entry is the repo's SPACE journal - scoped by construction,
    because the slug is in its path - so it reads unscoped. The global entry is
    read SCOPED by ``slug``. The global log is None when it cannot be scanned
    safely - no ``~/.fno`` or no resolvable git-remote slug. Without a slug
    nothing in the cross-project log can be attributed to this repo, so
    scanning it could only produce a cross-repo false positive.

    Separate from the scan so a caller that only wants to NAME the logs (the
    refusal text) does not read tens of MB of JSONL to learn two filenames.
    """
    root = _repo_root(cwd)
    if project_events is not None:
        project_path = project_events
    else:
        # project_log: producers write the space journal; a raw .fno path goes
        # quiet once any writer migrates it, and every PR then reads unreviewed.
        from fno.paths import project_log
        project_path = project_log("events.jsonl", project_root=root)
    try:
        from fno import paths as _paths

        # global_events_json(), not state_dir()/"events.jsonl": a RELATIVE
        # config.state_dir resolves into the repo checkout, while the global
        # journal deliberately falls back to ~/.fno - which is also the path the
        # Rust writer hardcodes. Reading state_dir() directly would scan a file
        # nobody writes on exactly those configs, silently restoring the bug.
        global_path = _paths.global_events_json()
        slug = _repo_identity(root)
    except Exception:  # noqa: BLE001 - no global log -> project log alone
        return project_path, None, None
    if not slug:
        return project_path, None, slug
    return project_path, global_path, slug


def coverage_sources(cwd: Optional[str] = None) -> list[str]:
    """The event logs a coverage read would consult, for a refusal message."""
    paths = _coverage_logs(cwd)[:2]
    return [str(p) for p in paths if p is not None]


def latest_review_coverage_row(
    pr_number: int,
    cwd: Optional[str] = None,
    project_events: Optional[Path] = None,
) -> tuple[Optional[dict], str]:
    """Latest ``review_coverage`` data for a PR and the ts it was written at.

    Returns ``(data, ts)``; ``(None, "")`` when neither log holds a match.

    Reads BOTH logs loop-check writes, because which one holds the attestation
    depends on where the reviewing session happened to stand. The stop hook
    writes into the events file of the directory it runs in, so a review
    attested inside a worktree lands in that worktree's project log; a merge run
    from canonical reads canonical's. They agreed only by luck, and the
    disagreement read as "nobody reviewed this". ``~/.fno`` is the one
    file both stand in, so it is the tiebreaker rather than a fallback.

    The project log is scanned unscoped (it is already repo-local); the global
    log is scoped by the full ``host/owner/repo`` identity. Newest ``ts`` wins
    across the two, so a
    project-only event from an older binary still beats a stale global one.

    A caller that refuses on a None names those logs with :func:`coverage_sources`:
    a gate that reports a count and not a location is what taught two workers to
    design around a green gate instead of looking at where it read.

    ``project_events`` overrides the derived project log for callers that hold
    an explicit path (the post-merge gate-escape detector, and its tests).
    """
    project_path, global_path, slug = _coverage_logs(cwd, project_events)

    best: Optional[dict] = None
    best_ts = ""
    if project_path is not None:
        best, best_ts = _scan_coverage(project_path, pr_number)

    if global_path is not None:
        other, other_ts = _scan_coverage(global_path, pr_number, repo_slug=slug)
        if other is not None and (
            best is None
            or other_ts > best_ts
            # Equal timestamps are common, not exotic: these are second-precision
            # and emit_to_both writes the two logs in the same instant. Strict
            # `>` would make "project log wins" the silent tiebreak, so a stale
            # covered event could outrank a same-second unknown and walk right
            # through a guard that exists to fail closed. On a tie take the
            # SAFER verdict instead of the arbitrary one.
            or (other_ts == best_ts and not _is_covered(other) and _is_covered(best))
        ):
            best, best_ts = other, other_ts

    return best, best_ts


def latest_review_coverage(
    pr_number: int,
    cwd: Optional[str] = None,
    project_events: Optional[Path] = None,
) -> Optional[dict]:
    """The row alone, for the callers that never needed its timestamp.

    A thin wrapper over :func:`latest_review_coverage_row`. The timestamp is
    separated rather than folded into the row because it belongs to the
    EVENT, not to the coverage data the producer wrote, and a caller deciding
    whether a stored answer has been overtaken needs to read it.
    """
    return latest_review_coverage_row(pr_number, cwd, project_events)[0]


def _exit4_degraded_reason(stdout: Optional[str]) -> str:
    """The exit-4 degradation reason from the verb's stdout, or ``""``.

    The verb's stdout is one JSON object whose last line is it even under
    stray warnings. Exit 4 means the read failed, so any ``reason`` the verb
    states is a real degradation cause - quota exhaustion (with its reset
    time) or a secondary-rate-limit refusal. Unparseable stdout, or a failure
    with no stated cause, keeps the empty string: the unknown row the verb
    emitted is still the caller's answer.
    """
    lines = [ln for ln in (stdout or "").splitlines() if ln.strip()]
    try:
        payload = json.loads(lines[-1]) if lines else None
    except ValueError:
        return ""
    if not isinstance(payload, dict):
        return ""
    reason = str(payload.get("reason") or "").strip()
    if payload.get("graphql_exhausted"):
        return reason or "graphql quota exhausted"
    return reason


def _exit4_reason_or_unstated(stdout: Optional[str]) -> str:
    """Exit 4's degradation reason, or the unstated-cause fallback.

    Exit 4 itself is the degradation signal: an outage with a healthy quota
    states no reason, but the read still failed, and an empty ``why`` would
    stamp a bare "recomputed" beside an unknown row - reading as "genuinely
    unreviewed" when the read in fact failed.
    """
    return _exit4_degraded_reason(stdout) or "gh read failed (exit 4)"


_PIN_PREFIX = "coverage row pinned to "


def split_pin_note(note: str) -> tuple[str, str]:
    """Split a gate note into ``(recompute_outcome, pinned_input)``.

    The two travel joined because a refusal renders one sentence, but they are
    different claims: one says what an instrument did, the other says what a
    stored answer rests on. A consumer that keys on the recompute must not
    receive the pin wearing its name.
    """
    parts = [part.strip() for part in (note or "").split(";") if part.strip()]
    pins = [p for p in parts if p.startswith(_PIN_PREFIX)]
    rest = [p for p in parts if not p.startswith(_PIN_PREFIX)]
    return "; ".join(rest), "; ".join(pins)


def _is_covered(data: Optional[dict]) -> bool:
    """Whether a coverage event reports covered, keyed on the WORD alone.

    The count is a fact beside the word, never a conjunct of it: a budget
    spent on fail rounds reads covered with passed_count 0, and every reader
    that tested ``covered AND count > 0`` reported zero reviews on a PR two
    rounds were spent reviewing."""
    if not data or data.get("coverage") != "covered":
        return False
    return True


def _is_optional(login: str, names: list[str]) -> bool:
    return bool(login) and _reviewer_matches(login, names)


def _fetch_threads(
    pr: str, slug: str, cwd: Optional[str], timeout: float, runner: Runner
) -> "Optional[list[tuple[str, bool, bool]]]":
    """Return [(author, is_resolved, is_outdated), ...] or None on failure."""
    owner, _, name = slug.partition("/")
    if not owner or not name:
        return None
    threads: list[tuple[str, bool, bool]] = []
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
        res = _quota.execute_graphql(
            "discretionary", args, runner=runner, cwd=cwd, timeout=timeout
        )
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
            resolved = node.get("isResolved")
            outdated = node.get("isOutdated")
            if not isinstance(resolved, bool) or not isinstance(outdated, bool):
                return None
            author = ""
            if cnodes and isinstance(cnodes[0], dict):
                author = (cnodes[0].get("author") or {}).get("login") or ""
            threads.append((author, resolved, outdated))
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
    """Compute optional review presence and thread counts for PR ``pr``.

    `optional_reviews`: list of `{author, state, inline_count}` for optional
    reviewers who posted, OR `"unknown"` on a read failure. `state` is the
    GitHub review state; `inline_count` is that author's review-thread count (a
    body-only COMMENTED review still lists, with `inline_count: 0`).

    `optional_reviews_unresolved`: count of unresolved (`isResolved == false`)
    threads authored by an optional reviewer - the headline actionable field
    (`green && unresolved == 0` == ready) - OR `None` on a read failure.

    `optional_reviews_resolved_unchanged`: count of resolved threads whose
    flagged line is still current (`isResolved == true && isOutdated == false`)
    for optional reviewers - OR `None` on a read failure.

    Both GraphQL reads route through the machine-wide quota broker. The broker
    holds one lock from the exempt quota probe through each command, so a fleet
    cannot race through the reserve after observing the same remaining point.
    """
    names = optional_reviewer_names(cwd)
    res = _quota.execute_graphql(
        "discretionary",
        ["pr", "view", pr, "--json", "reviews,url"],
        runner=runner,
        cwd=cwd,
        timeout=timeout,
    )
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
    resolved_unchanged = 0
    for author, resolved, outdated in threads:
        if not _is_optional(author, names):
            continue
        _entry(author)["inline_count"] += 1
        if resolved is False:
            unresolved += 1
        elif outdated is False:
            resolved_unchanged += 1

    return {
        "optional_reviews": sorted(by_author.values(), key=lambda e: e["author"]),
        "optional_reviews_unresolved": unresolved,
        "optional_reviews_resolved_unchanged": resolved_unchanged,
    }
