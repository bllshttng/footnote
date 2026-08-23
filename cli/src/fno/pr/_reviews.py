"""Optional-review signal for `fno do pr status` (x-705b).

x-d996 raised the drain-optional-review floor with SKILL.md prose, but prose is
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
import time
from pathlib import Path
from typing import Callable, Optional

from fno.graph._reconcile import repo_slug_from_url
from fno.pr import _quota
from fno.pr._proc import Result, run
from fno.pr_watch._discover import _reviewer_matches

Runner = Callable[..., Result]

_COUNTED_FRESHNESS = {
    "fresh",
    "carried_base_sync",
    "carried_docs_only",
    "carried_subset",
}
_KNOWN_REVIEW_VERDICTS = {"reviewed", "stale", "refused", "errored", "absent"}
_KNOWN_COVERAGE_PRODUCERS = {"github_app", "local_attestation"}
_KNOWN_REVIEW_STATES = {"reviewed", "unreviewed", "reviewer_refused"}

# The optional-reviewer bots the x-d996 drain paragraph names. config.review.peers
# (resolved below) extends this; config.review.required_bots is the separate GATE
# (read by loop-check) and is out of scope here.
_OPTIONAL_BOTS = ("gemini-code-assist", "chatgpt-codex-connector")

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
# Annotated because the values are heterogeneous (str, None, list) and mypy
# cannot infer a useful type from the literal alone.
_UNKNOWN_COVERAGE: dict[str, object] = {
    "coverage": "unknown",
    "reviewed_count": None,
    "self_attested_count": None,
    "head_sha": None,
    "stale_verdicts": [],
}

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
) -> tuple[Optional[Path], Optional[Path], Optional[str]]:
    """The logs a coverage read consults: ``(project, global, slug)``, either None.

    The project entry is read UNSCOPED (a repo-local file needs no scoping) and
    the global entry SCOPED by ``slug``. Both can be None; see the same-file case
    below, which is why the project entry is optional rather than always present.

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
        slug = _repo_identity(root)
    except Exception:  # noqa: BLE001 - no global log -> project log alone
        return project_path, None, None
    if global_path == project_path:
        # The two resolve to ONE file when the git top-level is $HOME (a
        # `git init ~` dotfiles checkout). The project log is normally read
        # unscoped because it is repo-local; here it IS the cross-project
        # journal, so an unscoped read would let any repo's PR N satisfy this
        # repo's guard. Drop the unscoped read and keep only the scoped one -
        # and with no identity to scope by, nothing here is safe to read.
        return None, (global_path if slug else None), slug
    if not slug:
        return project_path, None, slug
    return project_path, global_path, slug


def coverage_sources(cwd: Optional[str] = None) -> list[str]:
    """The event logs a coverage read would consult, for a refusal message."""
    paths = _coverage_logs(cwd)[:2]
    return [str(p) for p in paths if p is not None]


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

    return best


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


def _fire_review_coverage_verb(
    pr_number: int, cwd: Optional[str], head: Optional[str]
) -> tuple[bool, str]:
    """Run the ``fno-agents review-coverage`` verb once. Returns ``(ran, why)``.

    ``ran`` is True for any exit the verb defines (0/3/4) - including the
    unknown-coverage exit 4, which still emitted a row the caller re-reads.
    ``why`` names the failure when ``ran`` is False, for the refusal text; on
    exit 4 it carries the verb's degradation reason when stdout states one
    (an exhausted GraphQL quota with its reset time, or a secondary-rate-limit
    refusal), so a caller can tell "retriable after the reset" from "go get a
    review". ``why`` is advisory text callers interpolate, never a boolean.
    Binary resolution reuses :func:`fno.rust_binary.resolve_binary` (the one
    resolver; never a second lookup here).
    """
    import subprocess

    try:
        from fno import rust_binary

        binary = rust_binary.resolve_binary()
    except Exception:  # noqa: BLE001 - no resolver -> unavailable, fail closed
        return False, "fno-agents not found"
    if binary is None:
        return False, "fno-agents not found"
    argv = [
        str(binary),
        "review-coverage",
        "--cwd",
        str(_repo_root(cwd)),
        "--pr",
        str(pr_number),
    ]
    if head:
        argv += ["--head", head]
    try:
        proc = subprocess.run(argv, capture_output=True, text=True, timeout=120)
    except (OSError, subprocess.TimeoutExpired) as exc:
        return False, f"recompute failed: {exc}"
    if proc.returncode not in (0, 3, 4):
        # The verb prints its JSON (errors included) on stdout; stderr carries
        # anything the process itself choked on. Read both or an exit-2 refusal
        # names the exit with no cause.
        why = ((proc.stderr or "").strip() or (proc.stdout or "").strip()).splitlines()
        return False, f"recompute failed (exit {proc.returncode}: {why[-1] if why else ''})"
    if proc.returncode == 4:
        return True, _exit4_reason_or_unstated(proc.stdout)
    return True, ""


def review_coverage_for_gate(
    pr_number: int,
    cwd: Optional[str] = None,
    head: Optional[str] = None,
) -> tuple[Optional[dict], str]:
    """The coverage event a gate should act on, recomputed at most ONCE (x-3a3f).

    Reads :func:`latest_review_coverage`; when there is no usable row (nothing
    found, the found row pins a head that is not ``head``, or it says
    ``unknown``), fires the ``fno-agents review-coverage`` verb once - the same
    producer the stop hook uses - and re-reads once. Never more than one
    recompute per invocation: a loop here would turn a refusal into a spin. The
    ``unknown`` arm matters because the verb's own gh-failure exit writes
    exactly that row: without it one transient gh failure wedges the gate
    forever - a present, head-matching row no later merge ever recomputes.

    Every failure keeps the refusal: a binary that cannot be resolved, a
    non-zero exit, or a re-read that still yields nothing all return the
    original (or absent) row plus a note naming the recompute's outcome,
    because a refusal that reports only a count is what taught two workers to
    design around a gate that was green somewhere else.

    Returns ``(data_or_None, note)``; ``note`` is ``""`` when no recompute ran,
    else ``"recomputed"``, ``"recompute produced no row"``,
    ``"recompute unavailable: <why>"``, or - when the recompute ran but its gh
    read failed (exhausted quota, secondary limit, or an unstated cause) and
    the re-read row is still ``unknown`` - ``"recompute degraded to unknown:
    <why>"``, so a gate can say "retry after the quota reset" instead of a
    bare "nobody reviewed this".
    """
    raw = latest_review_coverage(pr_number, cwd)
    note = ""
    data = _shape_review_coverage(raw, head, cwd) if raw is not None else None
    ev_head = (raw or {}).get("head_sha")
    mismatch = bool(head and raw and ev_head and head != ev_head)
    unusable = data is not None and data.get("coverage") == "unknown"
    if raw is None or mismatch or unusable:
        ran, why = _fire_review_coverage_verb(pr_number, cwd, head)
        if ran:
            fresh = latest_review_coverage(pr_number, cwd)
            if fresh is not None:
                data = _shape_review_coverage(fresh, head, cwd)
                note = "recomputed"
                if why and data.get("coverage") == "unknown":
                    note = f"recompute degraded to unknown: {why}"
            else:
                note = "recompute produced no row"
        else:
            note = f"recompute unavailable: {why}"
    return data, note


def _is_covered(data: Optional[dict]) -> bool:
    """Whether a coverage event reports a real pass (covered AND count > 0)."""
    if not data or data.get("coverage") != "covered":
        return False
    try:
        return int(data.get("reviewed_count") or 0) > 0
    except (TypeError, ValueError):
        return False


def _reviewed_sha_is_ancestor(
    reviewed_sha: str, head: str, cwd: Optional[str]
) -> bool:
    """Whether Git proves that ``reviewed_sha`` remains in ``head`` history."""
    try:
        result = run(
            ["git", "merge-base", "--is-ancestor", reviewed_sha, head],
            cwd=cwd,
            timeout=5,
        )
    except Exception:  # noqa: BLE001 - an unreadable proof is not fresh
        return False
    return result.returncode == 0


def _pr_delta_patch_id(sha: str, base_ref: str, cwd: Optional[str]) -> Optional[str]:
    """Stable patch-id of the PR's own delta at ``sha`` against ``base_ref``.

    ``git diff base...sha`` names the branch's own changes (three-dot), not
    changes that landed on the base since the branch point, so the id stays
    comparable across a rebase that changed no content. ``None`` on any git
    failure or an empty diff: an absence is never evidence (the
    absence-matched-against-absence trap), matching the fail-closed contract
    of the Rust twin's ``pr_code_diff_identity``."""
    try:
        diff = run(
            ["git", "diff", f"{base_ref}...{sha}"],
            cwd=cwd,
            timeout=15,
        )
        if diff.returncode != 0 or not (diff.stdout or "").strip():
            return None
        pid = run(
            ["git", "patch-id", "--stable"],
            cwd=cwd,
            timeout=15,
            input_text=diff.stdout or "",
        )
    except Exception:  # noqa: BLE001 - an unreadable proof is not fresh
        return None
    if pid.returncode != 0:
        return None
    first = (pid.stdout or "").strip().splitlines()
    return first[0].split()[0] if first and first[0].split() else None


def _resolve_base_ref(cwd: Optional[str]) -> Optional[str]:
    """First resolvable default base ref, mirroring classify_payload in
    loopcheck.rs (origin/main, then origin/master). ``None`` leaves the
    content test unanswerable, which fails closed."""
    for ref in ("origin/main", "origin/master"):
        try:
            result = run(
                ["git", "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}"],
                cwd=cwd,
                timeout=5,
            )
        except Exception:  # noqa: BLE001 - next ref may still resolve
            continue
        if result.returncode == 0:
            return ref
    return None


def _reviewed_sha_still_describes_head(
    reviewed_sha: str, head: str, cwd: Optional[str]
) -> bool:
    """Whether the change the reviewer read still ships at ``head``.

    Ancestry is the cheap first test and covers every fast-forward. When it
    fails - a rebase rewrote every commit by construction - the CONTENT test
    takes over: the PR delta at ``reviewed_sha`` and at ``head`` must carry
    the same stable patch-id, so a rebase that changed nothing (x-e8db's
    treadmill) keeps the attestation while a conflict resolution that changed
    the delta loses it. Any unreadable input is not fresh, matching the
    existing ancestry contract."""
    if _reviewed_sha_is_ancestor(reviewed_sha, head, cwd):
        return True
    base_ref = _resolve_base_ref(cwd)
    if base_ref is None:
        return False
    reviewed_pid = _pr_delta_patch_id(reviewed_sha, base_ref, cwd)
    head_pid = _pr_delta_patch_id(head, base_ref, cwd)
    return (
        reviewed_pid is not None
        and head_pid is not None
        and reviewed_pid == head_pid
    )


def _verdicts_with_current_freshness(
    data: dict, head: Optional[str], cwd: Optional[str]
) -> list[dict]:
    """Copy verdicts and recheck stored freshness against current history.

    A freshness stamp describes the branch only when it was written. When a
    current head is available, the reviewed change must still ship at that
    head - by ancestry, or by an identical content delta when a rebase
    rewrote the commits (x-e8db: every rebase used to invalidate every
    attestation, even one that reviewed byte-identical content). Missing
    metadata or an unreadable ancestry/content result cannot prove freshness.
    """
    verdicts = data.get("verdicts")
    if not isinstance(verdicts, list):
        return []
    describes: dict[str, bool] = {}
    shaped: list[dict] = []
    for verdict in verdicts:
        if not isinstance(verdict, dict):
            continue
        current = dict(verdict)
        verdict_kind = verdict.get("verdict")
        stale = verdict_kind == "stale"
        reviewed_sha = verdict.get("reviewed_sha")
        if verdict_kind == "reviewed":
            stale = verdict.get("freshness") not in _COUNTED_FRESHNESS
        elif verdict_kind != "stale":
            current.pop("freshness", None)
        if verdict_kind == "reviewed" and head:
            if not isinstance(reviewed_sha, str) or not reviewed_sha:
                stale = True
            elif reviewed_sha not in describes:
                describes[reviewed_sha] = _reviewed_sha_still_describes_head(
                    reviewed_sha, head, cwd
                )
            if isinstance(reviewed_sha, str) and not describes.get(reviewed_sha, False):
                stale = True
        if stale:
            current["freshness"] = "stale"
        shaped.append(current)
    return shaped


def _stale_verdicts(verdicts: list[dict]) -> list[dict]:
    """Reviewers that responded against a commit that no longer describes HEAD.

    Each entry is ``{name, producer, reviewed_sha, freshness}``. The verdicts
    have already been normalized against current Git history, so older events
    with no freshness metadata fail closed instead of reading as fresh.
    """
    return [
        {
            "name": v.get("name"),
            "producer": v.get("producer"),
            "reviewed_sha": v.get("reviewed_sha"),
            "freshness": v.get("freshness"),
        }
        for v in verdicts
        if isinstance(v, dict)
        and v.get("verdict") in {"reviewed", "stale"}
        and v.get("freshness") == "stale"
    ]


def _derive_review_state(coverage: object, verdicts: object) -> str | None:
    """Derive one known outcome from validated per-reviewer verdicts."""
    if coverage == "unknown":
        return None
    if not isinstance(verdicts, list) or any(
        not isinstance(verdict, dict)
        or verdict.get("verdict") not in _KNOWN_REVIEW_VERDICTS
        or verdict.get("producer") not in _KNOWN_COVERAGE_PRODUCERS
        or not isinstance(verdict.get("name"), str)
        or not verdict.get("name")
        for verdict in verdicts
    ):
        return None
    if any(
        verdict.get("verdict") == "reviewed"
        and not verdict.get("human_approval", False)
        and verdict.get("freshness") in _COUNTED_FRESHNESS
        for verdict in verdicts
    ):
        return "reviewed"
    if any(verdict.get("verdict") == "refused" for verdict in verdicts):
        return "reviewer_refused"
    return "unreviewed"


def _shape_review_coverage(data: dict, head: Optional[str], cwd: Optional[str]) -> dict:
    """Shape one event and invalidate any unproven covered verdict."""
    shaped = dict(data)
    verdicts = _verdicts_with_current_freshness(data, head, cwd)
    shaped["verdicts"] = verdicts
    shaped["stale_verdicts"] = _stale_verdicts(verdicts)
    review_state = _derive_review_state(data.get("coverage"), verdicts)
    if review_state in _KNOWN_REVIEW_STATES:
        shaped["review_state"] = review_state
    else:
        shaped.pop("review_state", None)
    if data.get("coverage") != "covered":
        return shaped

    raw_verdicts = data.get("verdicts")
    malformed = (
        not isinstance(raw_verdicts, list)
        or not raw_verdicts
        or any(
            not isinstance(v, dict)
            or v.get("verdict") not in _KNOWN_REVIEW_VERDICTS
            or v.get("producer") not in _KNOWN_COVERAGE_PRODUCERS
            or not isinstance(v.get("name"), str)
            or not v.get("name")
            for v in raw_verdicts
        )
    )
    reviewed = [v for v in verdicts if v.get("verdict") == "reviewed"]
    valid = [v for v in reviewed if v.get("freshness") in _COUNTED_FRESHNESS]
    explicit_stale = any(v.get("verdict") == "stale" for v in verdicts)
    if malformed or explicit_stale or not reviewed or len(valid) != len(reviewed):
        shaped["coverage"] = "uncovered"
        shaped["reviewed_count"] = len(valid)
    return shaped


def review_coverage_for_head(
    pr_number: int, cwd: Optional[str], head: Optional[str]
) -> Optional[dict]:
    """Latest event shaped against the current head, without recomputing it."""
    data = latest_review_coverage(pr_number, cwd)
    return _shape_review_coverage(data, head, cwd) if data is not None else None


def read_review_coverage(
    pr_number: int,
    cwd: Optional[str] = None,
    head: Optional[str] = None,
    *,
    recompute: bool = False,
) -> dict:
    """The ``review_coverage`` verdict for a PR, recomputed once when there is
    no usable row and ``recompute`` is set (x-3a3f). The default stays a pure
    read so direct callers (and hermetic tests) never spawn a subprocess; the
    two gate surfaces - ``fno do pr merge`` and ``fno do pr status`` - opt in.
    Event-read failures degrade to the unknown sentinel. When ``head`` is
    supplied, verdict freshness fails closed unless Git proves the reviewed
    change still ships at that head (ancestry, or an identical content delta
    across a rebase - x-e8db).
    Python still consumes the event rather than recomputing coverage itself
    (Ownership: Rust computes, Python reads) - the recompute shells out to the
    SAME Rust producer the stop hook runs.

    Carries ``head_sha`` and ``stale_verdicts`` (x-5b99) so a reader can see
    WHICH commit was covered and by whom, and ``recompute`` (only when one ran)
    so a human report and the merge gate can name how the number arrived.
    """
    try:
        if recompute:
            latest, note = review_coverage_for_gate(pr_number, cwd, head)
        else:
            latest, note = review_coverage_for_head(pr_number, cwd, head), ""
    except Exception:  # noqa: BLE001 - additive signal, never hard-fails
        return dict(_UNKNOWN_COVERAGE)
    if latest is None:
        return dict(_UNKNOWN_COVERAGE)
    shaped = {
        "coverage": latest.get("coverage", "unknown"),
        "reviewed_count": latest.get("reviewed_count"),
        "self_attested_count": latest.get("self_attested_count"),
        "head_sha": latest.get("head_sha"),
        "stale_verdicts": latest.get("stale_verdicts", []),
    }
    if latest.get("review_state") in _KNOWN_REVIEW_STATES:
        shaped["review_state"] = latest["review_state"]
    # The raw verdict list rides along when present (older events carry none):
    # the local-pass conjunct scans it, and dropping it here made `fno do pr
    # status` refuse forever on a row `fno do pr merge` accepted (round 3, PR 917).
    if latest.get("verdicts") is not None:
        shaped["verdicts"] = latest["verdicts"]
    if note:
        shaped["recompute"] = note
    return shaped


# The commit-status context the repo ruleset requires. One name for
# the publisher, the refresher workflow, and the audit; a context string typed
# twice is a context that splits in two the first time one copy is edited.
COVERAGE_STATUS_CONTEXT = "fno/review-coverage"

# The label that makes an uncovered PR mergeable on purpose: the 3am release
# valve. Named here so the publisher, the refresher, and the docs agree on it.
COVERAGE_OVERRIDE_LABEL = "coverage-override"

# GitHub's commit-status description limit. Truncation keeps the head of the
# reason: the actionable half ("coverage was computed at X but HEAD is Y")
# leads, the tail is detail.
_GH_DESCRIPTION_LIMIT = 140

# Wait between status-POST attempts. Matches the refresher workflow and the
# stacked-base guard, which sleep the same 5s between their three attempts.
# A module const so a test can shrink it rather than pay the wait.
_POST_RETRY_SLEEP_SECS = 5.0


def _truncate_description(text: str, limit: int = _GH_DESCRIPTION_LIMIT) -> str:
    if len(text) <= limit:
        return text
    return text[: limit - 3] + "..."


def _override_label_actor(
    pr_number: int, repo: Optional[str], runner: Runner
) -> "tuple[bool, Optional[str]]":
    """Whether the PR carries the override label, and who applied it.

    The label read is ``gh pr view``; the actor comes from the issue events
    feed (a labeled event carries ``actor.login``), read only when the label is
    present. Any failure degrades to ``(False, None)`` - an unreadable label
    state must not mint a success status.

    The events read is PAGINATED. The feed is oldest-first at 30 per page, so
    an unpaginated read sees the oldest 30 events and never the label applied
    minutes ago: every busy PR would waive its review under "unknown actor",
    which is the one fact this receipt exists to carry. ``--paginate`` with
    ``--jq`` emits one line per page that matched, so the LAST line is the
    latest labeling.
    """
    res = runner(
        ["gh", "pr", "view", str(pr_number), "--json", "labels"], cwd=repo, timeout=30
    )
    if not res.ok:
        return False, None
    try:
        labels = [
            str(entry.get("name") or "")
            for entry in json.loads(res.stdout).get("labels", [])
        ]
    except (ValueError, TypeError):
        return False, None
    if COVERAGE_OVERRIDE_LABEL not in labels:
        return False, None
    actor: Optional[str] = None
    try:
        ev = runner(
            [
                "gh", "api", "--paginate",
                f"repos/:owner/:repo/issues/{pr_number}/events",
                "--jq",
                "[.[] | select(.event==\"labeled\" and .label.name==\""
                + COVERAGE_OVERRIDE_LABEL
                + "\")][-1].actor.login // empty",
            ],
            cwd=repo,
            timeout=30,
        )
    except Exception:  # noqa: BLE001 - the actor is cosmetic; the label stands
        # A slow events feed must not abort the publish: the label was already
        # confirmed, so post the override with an unnamed actor rather than
        # leaving an override-labelled PR with no status at all.
        return True, None
    if ev.ok and (ev.stdout or "").strip():
        actor = ev.stdout.strip().splitlines()[-1]
    return True, actor


def _best_effort_reviewed_count(pr_number: int, repo: Optional[str]) -> int:
    """The reviewed count off the raw row; 0 when unreadable.

    Description text only: the verdict above already gated on the count, so a
    failed re-read degrades the sentence, never the status state.
    """
    try:
        from fno.pr._merge import _safe_int

        row = latest_review_coverage(pr_number, repo)
    except Exception:  # noqa: BLE001 - cosmetic, never gates
        return 0
    return _safe_int((row or {}).get("reviewed_count"), 0)


def publish_coverage_status(
    pr_number: int,
    head: Optional[str] = None,
    cwd: Optional[str] = None,
    *,
    repo: Optional[str] = None,
    gate_verdict: Optional[tuple] = None,
) -> "tuple[bool, str]":
    """POST the coverage verdict as a commit status on the PR head.

    Reads with ``recompute=False``: a publisher must never spawn the 120s
    ``fno-agents`` subprocess. The caller has already caused the row to exist
    (the stop hook, the standalone verb, or ``fno do pr merge``'s own gate read).

    The verdict is the SAME predicate the merge gate enforces
    (``_coverage_gate.coverage_verdict``, imported, never restated): a status
    check that disagrees with the gate it certifies is worse than none.
    A caller that already holds the gate's answer (``fno do pr merge``) passes it
    as ``gate_verdict`` so the receipt stamps the decision that actually let
    the merge through, never a fresh read that may have flipped since.
    Success names the reviewed count and the sha it was computed at; failure
    carries the exact refusal text the local merge gate renders, so the person
    staring at the GitHub refusal reads the sentence a worker already
    recognises.

    Returns ``(posted, note)``; never raises. A failed POST is reported, not
    swallowed, but does not fail the caller - the gate lives on the GitHub
    side, where a missing status reads as "not passing" (fail-closed).
    """
    try:
        from fno.pr import _coverage_gate
        from fno.pr._merge import _pr_head_oid

        # One resolved directory for every gh/git spawn below. Never "" - an
        # empty cwd passed to subprocess is a crash, not "current directory".
        gh_dir = repo or cwd or str(Path.cwd())
        head = head or _pr_head_oid(pr_number, gh_dir)
        if not head:
            return False, "no PR head to publish a status on"

        runner: Runner = run
        if gate_verdict is None:
            verdict, refusal, covered_head, note = _coverage_gate.coverage_verdict(
                pr_number, gh_dir, recompute=False
            )
        else:
            verdict, refusal, covered_head, note = gate_verdict
        if verdict == _coverage_gate.COVERED and note.startswith(
            _coverage_gate.OVERRIDE_NOTE_PREFIX
        ):
            # Override first (AC5): a labelled PR publishes success naming the
            # label and its actor, legible in the PR timeline and the audit.
            # The gate read the label - the publisher does not read it a second
            # time, so the status it stamps and the verdict the merge enforces
            # cannot disagree about the valve.
            head = covered_head or head
            state = "success"
            description = note[len(_coverage_gate.OVERRIDE_NOTE_PREFIX) :]
        elif verdict == _coverage_gate.COVERED and covered_head:
            # POST on the head the row pins, not one the caller guessed
            # at: the verdict describes that sha and no other.
            head = covered_head
            count = _best_effort_reviewed_count(pr_number, gh_dir)
            state = "success"
            description = (
                f"covered: {count} reviewed at {head[:8]}"
                if count
                else f"covered at {head[:8]}"
            )
        elif verdict == _coverage_gate.COVERED and note == _coverage_gate.NO_LANE_NOTE:
            # No review lane configured: the merge gate does not apply.
            # Say so on the status instead of reading as a pass on
            # nothing. Keyed on the gate's OWN note, never on "covered_head is
            # empty": a covered row that carried no head_sha lands there too,
            # and telling the reader that a reviewed merge is ungated is the
            # receipt lying in the reassuring direction.
            state = "success"
            description = "no review lane configured; merge ungated"
        elif verdict == _coverage_gate.COVERED:
            # Covered, but the row pinned no head. The verdict still stands;
            # it just cannot name the sha it was computed at.
            count = _best_effort_reviewed_count(pr_number, gh_dir)
            state = "success"
            description = (
                f"covered: {count} reviewed (row pinned no head sha)"
                if count
                else "covered (row pinned no head sha)"
            )
        else:
            state = "failure"
            line = (
                _coverage_gate.refusal_line(refusal, note)
                if verdict == _coverage_gate.REFUSED
                else (note or "coverage verdict unavailable")
            )
            description = _truncate_description(line)

        # Three attempts, the same transient-5xx policy the refresher workflow
        # and the stacked-base guard apply to the same POST: once the ruleset
        # makes the context required, one blip here must not cost the merge
        # that follows it. The BACKOFF is the policy, not the count - three
        # POSTs fired back to back inside one millisecond all land in the same
        # outage, so a retry with no wait is a retry in name only. A permanent
        # 4xx costs two waits, which is the price of surviving the transient.
        args = [
            "gh", "api", "--method", "POST",
            f"repos/:owner/:repo/statuses/{head}",
            "-f", f"state={state}",
            "-f", f"context={COVERAGE_STATUS_CONTEXT}",
            "-f", f"description={description}",
        ]
        res = runner(args, cwd=gh_dir, timeout=30)
        for _retry in range(2):
            if res.ok:
                return True, ""
            time.sleep(_POST_RETRY_SLEEP_SECS)
            res = runner(args, cwd=gh_dir, timeout=30)
        if res.ok:
            return True, ""
        why = (res.stderr or res.stdout or f"gh exited {res.returncode}").strip()
        return False, why[:200]
    except Exception as exc:  # noqa: BLE001 - a publisher must never raise
        return False, f"publish failed: {exc}"


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
