"""Backlog PR-merge drift detection for ``fno backlog reconcile``.

Pure logic split from I/O, mirroring ``fno.megatron.reconcile``. The
GitHub query is the only I/O dependency; tests stub ``query_pr_merge_state``
where its callers run.

The completion ritual (stamp plan -> mark node ``done`` -> capture follow-ups)
runs automatically only when work flows through ``/target``'s ship gate or
``scripts/lib/pr-merge.sh``. A PR merged any other way (manual GitHub merge,
bare ``gh pr merge``) leaves the node open. This module detects that drift so
the CLI can close it mechanically, no memory required.
"""
from __future__ import annotations

import hashlib
import json
import logging
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from functools import lru_cache
from pathlib import Path
from typing import Any, Callable, Iterable, Literal, Optional

log = logging.getLogger(__name__)

# `gh pr view` default timeout; covers network hangs and stuck auth prompts.
GH_QUERY_TIMEOUT_S = 30.0

@lru_cache(maxsize=1)
def _gh_executable() -> Optional[str]:
    """Resolve the gh binary path once per process.

    query_pr_merge_state runs once per PR, so a bare shutil.which("gh")
    would re-scan PATH on every node. Cached here.
    """
    return shutil.which("gh")

# `owner/repo` + PR number from a GitHub PR/issue URL. The host is anchored,
# never searched for: a substring match accepts notgithub.com and
# gitlab.com/mirrors/github.com/o/r, handing back a confident `o/r` that names
# a repo the url does not belong to. The trailing delimiter on the number
# rejects `/pull/123suffix`. A trailing `.git` is never present on web URLs but
# stripped defensively.
_PR_URL_RE = re.compile(
    r"^(?:[A-Za-z][A-Za-z0-9+.-]*://)?(?:[^/@]*@)?github\.com(?::\d+)?"
    r"/([^/\s]+)/([^/\s]+?)(?:\.git)?/(?:pull|issues)/(\d+)(?:[/?#]|$)",
    re.IGNORECASE,
)


def repo_slug_from_url(url: Optional[str]) -> Optional[str]:
    """Return ``owner/repo`` parsed from a GitHub PR URL, or None.

    Used to scope ``gh pr view`` to the node's actual repository via
    ``--repo``. Without this, gh resolves the PR number against whatever repo
    the process happens to run in, so a same-number PR in a different repo
    could be mistaken for the node's PR.
    """
    if not url:
        return None
    m = _PR_URL_RE.match(url.strip())
    return f"{m.group(1)}/{m.group(2)}" if m else None


def _slug_from_remote(raw: str) -> Optional[str]:
    """``owner/repo`` from a git remote URL, or None for a non-GitHub remote.

    Handles every form git accepts - scp (``git@github.com:o/r.git``), https,
    ``ssh://…:22/o/r.git``, trailing slash - by stripping scheme, credentials
    and port before anchoring on the host. The host check must be exact and
    the path exactly two segments deep: an unanchored match reads
    ``ssh://git@github.com:22/o/r.git`` as the repo ``22/o/r`` and a GitLab
    mirror path as ``o/r``, and the writer then persists that wrong url.
    """
    s = re.sub(r"^[A-Za-z][A-Za-z0-9+.-]*://", "", raw.strip())
    s = re.sub(r"^[^/@]*@", "", s)
    m = re.match(r"github\.com(?::\d+)?[:/](.+)$", s, re.IGNORECASE)
    if not m:
        return None
    path = m.group(1).rstrip("/")
    if path.endswith(".git"):
        path = path[:-4]
    parts = path.split("/")
    if len(parts) != 2 or not all(parts):
        return None
    return f"{parts[0]}/{parts[1]}"


def _run_slug_cmd(argv: list, cwd: Optional[str]) -> "tuple[int, str]":
    try:
        proc = subprocess.run(
            argv, cwd=cwd, capture_output=True, text=True, timeout=GH_QUERY_TIMEOUT_S
        )
    except (OSError, subprocess.SubprocessError):
        return 127, ""
    return proc.returncode, proc.stdout


def resolve_current_repo_slug(
    cwd: Optional[str] = None,
    *,
    runner: Callable[..., "tuple[int, str]"] = _run_slug_cmd,
) -> Optional[str]:
    """Best-effort ``owner/repo`` for the checkout at ``cwd``, or None.

    git origin first, then ``gh repo view``: the git read needs no network and
    no auth, and a repo whose origin parses is the overwhelming majority case.
    gh still covers the checkout whose GitHub remote is not named ``origin``.
    None on every failure - the caller degrades to unscoped resolution, which
    is a safe skip on ambiguity, never a wrong stamp.
    """
    rc, out = runner(["git", "remote", "get-url", "origin"], cwd)
    if rc == 0:
        slug = _slug_from_remote(out.strip())
        if slug:
            return slug
    rc, out = runner(
        ["gh", "repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"], cwd
    )
    return (out.strip() or None) if rc == 0 else None


def pr_number_from_url(url: Optional[str]) -> Optional[int]:
    """The PR number a GitHub PR url names, or None.

    A url that carries the wrong number is worse than none: the row then points
    reconciliation at one PR and renders a link to another, and repo-scoped
    matching - which compares BOTH slug and number - finds neither.
    """
    if not url:
        return None
    m = _PR_URL_RE.match(url.strip())
    return int(m.group(3)) if m else None


def pr_url_from_slug(slug: str, pr: int) -> str:
    """The one place a PR url is built, so ``repo_slug_from_url`` round-trips it."""
    return f"https://github.com/{slug}/pull/{pr}"


_REPO_SLUG_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")


def is_repo_slug(value: str) -> bool:
    """True for a bare ``owner/repo`` slug, the shape ``--repo`` accepts.

    ``repo_slug_from_url`` answers a different question (it parses a full PR
    url), so a caller letting the operator NAME a repo needs its own shape
    check: a url, a bare repo name, or a slug with a third segment must refuse
    rather than build a url ``repo_slug_from_url`` cannot round-trip.
    """
    return bool(_REPO_SLUG_RE.match((value or "").strip()))


def pr_url_for_repo(
    pr: int,
    cwd: Optional[str] = None,
    *,
    runner: Callable[..., "tuple[int, str]"] = _run_slug_cmd,
) -> Optional[str]:
    """The canonical PR url for the checkout at ``cwd``, or None.

    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    if cwd:
        cwd = os.path.expanduser(cwd)
        if not os.path.isdir(cwd):
            print(
                f"note: recorded cwd {cwd} is gone - refusing to resolve PR "
                f"#{pr} against a different checkout",
                file=sys.stderr,
            )
            return None
    slug = resolve_current_repo_slug(cwd or None, runner=runner)
    return pr_url_from_slug(slug, pr) if slug else None

# GitHub PR states we resolve from gh, plus the "UNKNOWN" sentinel used on a
# drift record whose query failed. Kept here as the single home for the
# vocabulary so both types below reference it rather than bare strings.
PrStateLiteral = Literal["OPEN", "CLOSED", "MERGED", "UNKNOWN"]


class ReconcileError(Exception):
    """A PR-state read failure with a machine-readable operator remedy."""

    def __init__(
        self,
        message: str,
        *,
        kind: Optional[str] = None,
        remedy: Optional[str] = None,
    ) -> None:
        super().__init__(message)
        self.kind = kind or self._classify(message)
        self.remedy = remedy

    @staticmethod
    def _classify(message: str) -> str:
        text = message.lower()
        if "not found on path" in text or "toolmissing" in text:
            return "availability"
        if (
            "unconditional" in text
            or "routed, never rationed" in text
            or "graphql reserve" in text
            or "use `fno do pr info" in text
        ):
            return "routing_refusal"
        if (
            re.search(r"\b(?:auth|authentication|authenticated|credentials)\b", text)
            or "bad credentials" in text
            or "not logged in" in text
            or re.search(r"\b(?:http|status)\s*401\b", text)
        ):
            return "authentication"
        if "could not resolve owner/repo" in text or "no repo context" in text:
            return "repository_context"
        if (
            "not found" in text
            or "404" in text
            or "could not resolve to a repository" in text
            or "could not resolve to a pullrequest" in text
        ):
            return "not_found"
        if "3,000-file cap" in text:
            return "evidence_incomplete"
        if "rate limit" in text or "quota" in text:
            return "availability"
        if (
            re.search(r"\b(?:http|status)\s*5\d{2}\b", text)
            or "bad gateway" in text
            or "internal server error" in text
        ):
            return "availability"
        if any(
            token in text
            for token in ("not json", "malformed", "parse", "json value", "no output")
        ):
            return "malformed"
        if any(
            token in text
            for token in ("timeout", "timed out", "network", "transport", "connection", "unavailable")
        ):
            return "availability"
        return "reader_error"

    @property
    def retryable(self) -> bool:
        return self.kind == "availability"

    def remedy_for(self, *, pr_number: int, repo: Optional[str]) -> str:
        if self.remedy:
            return self.remedy
        if self.kind == "routing_refusal":
            scope = f" --repo {repo}" if repo else ""
            return (
                f"Use `fno do pr info {pr_number}{scope}` as the route. "
                "This refusal is unconditional and is not retryable."
            )
        if self.kind == "authentication":
            return "Run `gh auth login`; authentication failures are not retryable."
        if self.kind == "not_found":
            stored = f"{repo}#{pr_number}" if repo else f"PR #{pr_number}"
            return (
                f"Stored PR reference {stored} was not found. Repair it with "
                "`fno backlog update <node> pr_url=<correct-url>`; do not "
                "retry against the ambient repository."
            )
        if self.kind == "malformed":
            return "Inspect the malformed GitHub response and fix the reader; this is not retryable."
        if self.kind == "evidence_incomplete":
            return (
                "GitHub's REST files endpoint reached its 3,000-file cap without "
                "a complete tail; review the full PR evidence before using "
                "`fno backlog done <node> --force --reason TEXT`."
            )
        if self.kind == "reader_error":
            return "The REST reader raised an unexpected error; inspect the reader before retrying."
        if self.kind == "repository_context":
            return (
                "Supply `--repo <owner/repo>` or repair the checkout's origin remote; "
                "repository context is required and this is not retryable."
            )
        if self.kind == "availability":
            if "rate limit" in str(self).lower() or "quota" in str(self).lower():
                return "Back off GitHub API requests and retry after the rate limit clears."
            return "Retry the GitHub read when availability returns; the node stays open."
        return "The PR read failed for an unclassified reason; inspect it before retrying."


@dataclass
class PrMergeState:
    number: int
    # PrStateLiteral rather than the 3-value subset: query_pr_merge_state
    # falls back to "UNKNOWN" when gh omits the state field.
    state: PrStateLiteral
    url: Optional[str]
    merged_at: Optional[str]
    # mergeCommit.oid - the dedup key for post-merge-ritual auto-dispatch
    #. Optional: absent on a non-merged PR or when gh omits it.
    merge_sha: Optional[str] = None
    changed_files: list[str] = field(default_factory=list)
    files_truncated: bool = False


@dataclass
class MergeDriftRecord:
    """One open node carrying a PR whose GitHub state we resolved.

    ``closeable`` records hold a MERGED PR and are safe to close. Records with
    a non-None ``error`` could not be resolved (gh failure) and are surfaced
    but never closed.
    """

    node_id: str
    plan_path: Optional[str]
    pr_number: int
    pr_url: Optional[str]
    pr_state: PrStateLiteral
    merged_at: Optional[str]
    error: Optional[str] = None
    # The owning target session + its working directory, carried from the graph
    # node so the CLI can emit a session_satisfied event for that session after
    # closing the node (Group 1 /). Both optional: a node may have
    # been intaken without a session (session_id null) or without a cwd.
    session_id: Optional[str] = None
    cwd: Optional[str] = None
    # mergeCommit.oid for the closed PR - the exactly-once dedup key for the
    # post-merge-ritual auto-dispatch. None on a reverse-mapped record
    # (branch-name match has no SHA); the dispatcher falls back to a pr-number
    # key there.
    merge_sha: Optional[str] = None
    changed_files: list[str] = field(default_factory=list)
    files_truncated: bool = False
    error_kind: Optional[str] = None
    remedy: Optional[str] = None

    @property
    def closeable(self) -> bool:
        return self.error is None and self.pr_state == "MERGED"


def _normalize_surface(path: object) -> str:
    """One spelling for a repo-relative path on both sides of the match.

    ``removeprefix`` and not ``lstrip("./")``: lstrip strips a character SET, so
    it ate the leading dot of every dotfile path and let a declared
    ``github/ci.yml`` falsely match a changed ``.github/ci.yml``.
    """
    return str(path).strip().replace("\\", "/").removeprefix("./")


def verify_pending_supersessions(
    entries: list[dict],
    *,
    successor: str,
    changed_files: Iterable[str],
    evidence_pr: int,
    verified_at: Optional[str] = None,
    evidence_complete: bool = True,
) -> list[dict]:
    """Verify predecessor cause surfaces against one merged PR's file set.

    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    changed = {
        _normalize_surface(path)
        for path in changed_files
        if isinstance(path, str) and path.strip()
    }
    receipts: list[dict] = []
    stamp = verified_at or datetime.now(timezone.utc).isoformat()
    for entry in entries:
        if not isinstance(entry, dict) or entry.get("superseded_by") != successor:
            continue
        record = entry.get("supersession")
        if not isinstance(record, dict) or record.get("verified_at"):
            continue
        surfaces = [
            _normalize_surface(surface)
            for surface in record.get("surfaces") or []
            if isinstance(surface, str) and surface.strip()
        ]
        matched = [surface for surface in surfaces if surface in changed]
        uncovered = [surface for surface in surfaces if surface not in changed]
        if uncovered and not evidence_complete:
            receipts.append({
                "kind": "supersession_evidence_truncated",
                "predecessor": entry.get("id"),
                "successor": successor,
                "cause": record.get("cause"),
                "uncovered_surfaces": uncovered,
                "evidence_pr": evidence_pr,
            })
            continue
        if uncovered:
            receipts.append({
                "kind": "supersession_unverified",
                "predecessor": entry.get("id"),
                "successor": successor,
                "cause": record.get("cause"),
                "uncovered_surfaces": uncovered,
                "evidence_pr": evidence_pr,
            })
            continue
        record.update({
            "verified_at": stamp,
            "evidence_pr": evidence_pr,
            "matched_surfaces": matched,
        })
    return receipts


def successors_owing_verification(entries: list[dict]) -> dict[str, dict]:
    """Successor id -> successor node, for pending predecessors already owed proof.

    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    by_id = {
        entry.get("id"): entry
        for entry in entries
        if isinstance(entry, dict) and isinstance(entry.get("id"), str)
    }
    owed: dict[str, dict] = {}
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        successor_id = entry.get("superseded_by")
        record = entry.get("supersession")
        if not successor_id or not isinstance(record, dict) or record.get("verified_at"):
            continue
        successor = by_id.get(successor_id)
        # Only a CLOSED successor is owed here. An open one still has its
        # ordinary close ahead of it, and that path already verifies.
        if not isinstance(successor, dict) or not successor.get("completed_at"):
            continue
        if isinstance(successor.get("pr_number"), int):
            owed[successor_id] = successor
    return owed


def node_is_open(node: dict) -> bool:
    """A node is open when it is neither done nor superseded.

    Keyed off the underlying fields rather than the derived ``status`` so the
    predicate holds even on an entries list that has not been through
    ``recompute_statuses`` (e.g. a raw test fixture).
    """
    if node.get("completed_at"):
        return False
    successor = node.get("superseded_by")
    if not successor:
        return True
    record = node.get("supersession")
    return isinstance(record, dict) and not record.get("verified_at")


def node_pr_refs(node: dict) -> list[tuple[int, Optional[str]]]:
    """Return ``(pr_number, pr_url)`` pairs for a node, primary first.

    Combines the primary ``pr_number``/``pr_url`` with any ``additional_prs``
    entries. De-duplicates by number (primary wins) so a PR listed in both
    places is queried once.
    """
    refs: list[tuple[int, Optional[str]]] = []
    seen: set[int] = set()

    primary = node.get("pr_number")
    if isinstance(primary, int):
        refs.append((primary, node.get("pr_url")))
        seen.add(primary)

    for extra in node.get("additional_prs") or []:
        if not isinstance(extra, dict):
            continue
        num = extra.get("number")
        if not isinstance(num, int) or num in seen:
            continue
        refs.append((num, extra.get("url")))
        seen.add(num)

    return refs


# The exit-code contract, owned here so no verb can fork it. The loop runtime
# keys on 5 for AwaitingMerge, so these numbers are load-bearing: do not renumber.
REFUSAL_EXIT_CODES = {"awaiting_merge": 5, "outage": 4, "refused": 3}


@dataclass
class MergeEvidence:
    """The close decision for a node's PR refs, shared by every done verb."""

    outcome: Literal["merged", "awaiting_merge", "outage", "refused"]
    pr_url: Optional[str] = None
    open_pr_number: Optional[int] = None
    error: Optional[str] = None
    reason: Optional[str] = None
    failure_kind: Optional[str] = None
    remedy: Optional[str] = None

    @property
    def exit_code(self) -> int:
        """The exit code this outcome must produce. 0 when it closes."""
        return REFUSAL_EXIT_CODES.get(self.outcome, 0)


def resolve_merge_evidence(
    refs: list[tuple[int, Optional[str]]],
    *,
    cwd: Optional[str] = None,
    query: Optional[Callable[..., PrMergeState]] = None,
) -> MergeEvidence:
    """Decide whether a node's PR refs are closing evidence.

    The first MERGED ref closes; an OPEN ref means awaiting merge and outranks
    an outage, because a live PR is a definite answer where an unreachable ref
    is not. CI state is deliberately not consulted: green CI is the session's
    finish line, not the graph's close condition.

    ``query`` is injected so callers can pass their own monkeypatchable shim.
    """
    query = query or query_pr_merge_state
    if not refs:
        return MergeEvidence(outcome="refused", reason="no PR ref to evidence")
    refusal_reason: Optional[str] = None
    refusal_kind: Optional[str] = None
    refusal_remedy: Optional[str] = None
    outage_error: Optional[str] = None
    outage_remedy: Optional[str] = None
    open_pr_number: Optional[int] = None
    first_pr_number, first_pr_url = refs[0]
    repo = repo_slug_from_url(first_pr_url)

    for pr_number, pr_url in refs:
        pr_repo = repo_slug_from_url(pr_url) or repo
        pr_cwd = cwd if pr_repo is None else None

        try:
            pr_state = query(pr_number, repo=pr_repo, cwd=pr_cwd)
        except ReconcileError as exc:
            log.warning(
                "merge evidence read failed for #%s repo=%s kind=%s: %s",
                pr_number,
                pr_repo,
                exc.kind,
                exc,
            )
            if exc.retryable:
                outage_error = str(exc)
                outage_remedy = exc.remedy_for(pr_number=pr_number, repo=pr_repo)
            elif refusal_kind is None:
                refusal_reason = str(exc)
                refusal_kind = exc.kind
                refusal_remedy = exc.remedy_for(pr_number=pr_number, repo=pr_repo)
            continue

        if pr_state.state == "MERGED":
            return MergeEvidence(outcome="merged", pr_url=pr_url)

        if pr_state.state == "OPEN":
            if open_pr_number is None:
                open_pr_number = pr_number
        else:
            if refusal_reason is None and refusal_kind is None:
                refusal_reason = f"PR #{pr_number} state={pr_state.state} (not merged)"

    if refusal_kind is not None:
        return MergeEvidence(
            outcome="refused",
            reason=refusal_reason,
            failure_kind=refusal_kind,
            remedy=refusal_remedy,
        )
    if open_pr_number is not None:
        # Carry any outage alongside: a ref we could not reach stays invisible
        # across every retry otherwise, since OPEN outranks it.
        return MergeEvidence(
            outcome="awaiting_merge",
            open_pr_number=open_pr_number,
            error=outage_error,
            remedy=outage_remedy,
        )
    if outage_error:
        return MergeEvidence(
            outcome="outage", error=outage_error, remedy=outage_remedy,
        )
    return MergeEvidence(
        outcome="refused",
        reason=refusal_reason or f"PR #{first_pr_number}: no merged evidence",
        failure_kind=refusal_kind,
        remedy=refusal_remedy,
    )


def render_merge_evidence_failure(
    task_id: str,
    evidence: MergeEvidence,
    *,
    stays: Literal["open", "done"],
) -> str:
    """Render the shared actionable outcome for every close front door."""
    if evidence.outcome == "outage":
        action = evidence.remedy or "Retry the GitHub read when availability returns."
        return (
            f"Error: gh cross-check failed for {task_id}: {evidence.error}\n"
            f"{action} Node stays {stays}."
        )
    message = evidence.reason or "no merged evidence"
    if evidence.remedy:
        return f"Refused: {task_id} cross-check failed: {message}\n{evidence.remedy}"
    return f"Refused: {task_id} cross-check failed: {message}"


# The promise-gate refusal code. 3/4/5 are taken and load-bearing
# (REFUSAL_EXIT_CODES above; the loop runtime keys on 5), so this is the next
# free slot across graph/cli.py and done/cli.py. Do not renumber.
PROMISE_REFUSAL_EXIT_CODE = 6

# A retryable read outage is not a policy refusal, so it does not reuse 6. It
# reuses the merge gate's own outage code so one number means "the reader was
# down, retry" across both gates.
PROMISE_UNKNOWN_EXIT_CODE = REFUSAL_EXIT_CODES["outage"]

# Wall clock for the fno-agents probe-run subprocess (3 probes * 60s native
# timeout + overhead). resolve_promise_evidence runs outside the graph lock,
# same as the gh cross-check, so a slow probe holds no other mutation.
PROBE_RUN_TIMEOUT_S = 240.0


@dataclass
class PromiseVerdict:
    """The plan-closure promise decision for a node, shared by every close verb.

    Sibling to :class:`MergeEvidence`: the merge gate asks "is a PR merged" (a
    fact about an artifact); this asks "did the plan's declared work all ship".
    One verdict, three callers (``cmd_done``, ``cmd_reconcile``, ``done_command``)
    so a node can never close through a second, ungated path. An absent
    ``plan_path`` promises nothing and passes; an unreadable or unparseable
    plan is a delivery promise that cannot be confirmed, so it fails CLOSED
    (``promise_unmet``) with the remedy in the refusal - never as a
    silent pass.
    """

    outcome: Literal["ok", "promise_unmet", "promise_unknown"]
    reason: Optional[str] = None  # multi-line refusal text; None when ok
    # Why a degraded read passed anyway (today only the prove-it verdict
    # reader). Returned, not printed: the close verbs emit it on stderr, but
    # a raw stderr write here would pollute the `reconcile --json` stream
    # (JSON on stdout) and break the JSON-only contract of that path.
    warning: Optional[str] = None

    @property
    def satisfied(self) -> bool:
        """True only on positive evidence. Every close boundary reads THIS.

        A negative test (``outcome == "promise_unmet"``) closed the node on any
        outcome the author had not enumerated, which is how a retryable read
        outage closed a declared multi-ship node.
        """
        return self.outcome == "ok"

    @property
    def exit_code(self) -> int:
        if self.outcome == "promise_unmet":
            return PROMISE_REFUSAL_EXIT_CODE
        if self.outcome == "promise_unknown":
            return PROMISE_UNKNOWN_EXIT_CODE
        return 0


def _close_probe_runner_shellout(
    plan_path: str, cwd: Optional[str]
) -> tuple[bool, str]:
    """Default ``probe_runner``: shell out to ``fno-agents probe-run``.

    Returns ``(passed, detail)``. ``detail`` is empty on pass and a single-line
    reason on fail. Fail-closed: a binary absent or a verb too old to know
    ``probe-run`` refuses rather than reading a declared gate as no gate.
    """
    # resolve_binary, not shutil.which: the wheel ships fno-agents inside the
    # package (`<pkg>/_bin/`) and `$FNO_AGENTS_BIN` overrides it, neither of
    # which is on PATH. A bare which() plus this function's fail-closed contract
    # would refuse EVERY close_probes close on a plain `pip install fno`.
    from fno.rust_binary import resolve_binary

    exe = resolve_binary()
    if not exe:
        return (
            False,
            "close_probes declared but the fno-agents binary was not found "
            "(set FNO_AGENTS_BIN or run `fno doctor update --rust`); "
            "the gate cannot be evaluated",
        )
    cmd = [
        str(exe),
        "probe-run",
        "--plan",
        plan_path,
        "--key",
        "close_probes",
        "--json",
    ]
    if cwd:
        cmd += ["--cwd", cwd]
    try:
        proc = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=PROBE_RUN_TIMEOUT_S,
            cwd=cwd or None,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return (False, f"close_probes runner timed out after {PROBE_RUN_TIMEOUT_S}s")
    except OSError as exc:
        return (False, f"close_probes runner failed to launch: {exc}")
    if proc.returncode == 0:
        return (True, "")
    # Non-zero: prefer the structured reason; fall back to fail-closed when the
    # verb itself errored (clap writes an unrecognized-subcommand notice to
    # stderr and emits no JSON).
    try:
        payload = json.loads(proc.stdout or "{}")
        reason = payload.get("reason") or "a declared close_probe exited non-zero"
    except json.JSONDecodeError:
        stderr_tail = (proc.stderr or "").strip()
        reason = (
            f"close_probes declared but `fno-agents probe-run` did not evaluate "
            f"them (rc={proc.returncode}): {stderr_tail[:200] or 'no diagnostic'}"
        )
    return (False, reason)


def _verdict_reader_shellout(plan_path: str) -> "dict[str, Any]":
    """Default ``verdict_reader``: one round-trip with ``fno-agents
    prove-it-verdicts``; a ``VerbUnavailable`` degrades to a warning, the
    same posture a plan read failure takes."""
    from fno.rust_binary import verb_call

    return verb_call("prove-it-verdicts", {})


def resolve_promise_evidence(
    node: dict,
    *,
    cwd: Optional[str] = None,
    query: Optional[Callable[..., PrMergeState]] = None,
    probe_runner: Optional[Callable[[str, Optional[str]], tuple[bool, str]]] = None,
    extra_refs: Optional[list[tuple[int, Optional[str]]]] = None,
    carveout_reader: Optional[Callable[[Optional[str]], "list[dict]"]] = None,
    verdict_reader: Optional[Callable[[str], "dict[str, Any]"]] = None,
) -> PromiseVerdict:
    """Decide whether a node's plan promised work that has not all shipped.

    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    # Condition D (checked first; independent of the plan): an unharvested
    # `deferred` carve-out is declared scope that did not ship, and it blocks
    # the close until it becomes a node (harvest) or is force-overridden. Only
    # the closing node's OWN rows (``node`` field, stamped at capture time):
    # a repo-wide read held every close open on any unrelated row.
    # See the docstring for why unattributed rows block nothing.
    node_id = str(node.get("id") or "")
    deferred = [
        r
        for r in (carveout_reader or _unharvested_deferred_carveouts)(cwd)
        if r.get("node") == node_id
    ]
    if deferred:
        return PromiseVerdict(
            outcome="promise_unmet",
            reason=_promise_refusal_d(node_id or "(unknown)", deferred),
        )

    plan_path = node.get("plan_path")
    if not isinstance(plan_path, str) or not plan_path:
        return PromiseVerdict(outcome="ok")

    # The Python plan readers strip a `#wave-1` fragment; reading the literal
    # name would fail and silently drop the gate.
    plan_path_clean = plan_path.split("#", 1)[0]
    plan_file = Path(plan_path_clean)
    if not plan_file.is_absolute():
        # A repo-relative plan_path belongs to the node's own checkout, not the
        # caller's CWD: the global reconcile runs from canonical while a node's
        # plan lives in its owning worktree/vault. Resolve against `cwd` (passed
        # by every close path) the way the Rust probe parser does - otherwise the
        # read fails and the gate fails open silently (a decorative no-op).
        if isinstance(cwd, str) and cwd:
            plan_file = Path(cwd) / plan_path_clean
    try:
        text = plan_file.read_text(encoding="utf-8")
    except OSError as exc:
        return PromiseVerdict(
            outcome="promise_unmet",
            reason=(
                f"{node_id or '(unknown)'}: the promise gate could not read plan "
                f"{plan_path_clean} ({exc}). An unreadable delivery promise is "
                f"never a complete one. Fix or restore the plan, then close; "
                f"or close with --force --reason."
            ),
        )

    from fno.plan._doc import FrontmatterError, _parse_frontmatter, _split_frontmatter

    # Only the frontmatter is read here (close_probes, expected_url_count); the
    # body is the probe-runner's to parse via fno-agents probe-run.
    yaml_text, _body = _split_frontmatter(text)
    try:
        frontmatter = _parse_frontmatter(yaml_text) if yaml_text.strip() else {}
    except FrontmatterError as exc:
        return PromiseVerdict(
            outcome="promise_unmet",
            reason=(
                f"{node_id or '(unknown)'}: plan frontmatter would not parse "
                f"({exc}) in {plan_path_clean}. An unparseable delivery promise "
                f"never reads as complete. Fix the frontmatter, then close; "
                f"or close with --force --reason."
            ),
        )

    close_probes = frontmatter.get("close_probes")
    expected_raw = frontmatter.get("expected_url_count")
    expected = expected_raw if isinstance(expected_raw, int) else None
    node_id = node.get("id", "(unknown)")
    plan_display = node.get("plan_path", plan_path_clean)

    # An Execution Strategy that declares waves promises one ship per wave
    # (the multi-wave stamp bug: wave 1's merge closed a 3-wave node).
    if expected is None:
        m = re.search(r"^## +Execution Strategy\b.*?$(.*?)(?=^## +|\Z)", _body, re.M | re.S)
        if m is not None:
            try:
                from fno.plan.brief import parse_execution_strategy

                waves = parse_execution_strategy(m.group(1)).get("waves")
            except Exception:  # noqa: BLE001 - unread skips the gate, never blocks
                waves = None
            if isinstance(waves, list) and len(waves) >= 2:
                expected = len(waves)

    # Condition E: an open prove-it FAIL on this node's own plan
    # artifacts is claimed work whose outcome did not hold; it needs no
    # declaration. A done node is never reopened; a failed reader is a warning.
    if node_id:
        try:
            payload = (verdict_reader or _verdict_reader_shellout)(plan_path_clean)
        except Exception as exc:  # noqa: BLE001 - a failed read must not block a close
            return PromiseVerdict(
                outcome="ok",
                warning=f"prove-it verdict read failed ({exc}); gate skipped for this close",
            )
        fail_rows = [
            r for r in (payload.get("rows") or [])
            if isinstance(r, dict) and r.get("node") == node_id and r.get("open")
        ]
        if fail_rows:
            row = fail_rows[0]
            return PromiseVerdict(
                outcome="promise_unmet",
                reason=(
                    f"{node_id}: prove-it FAIL on its own plan artifacts "
                    f"({row.get('report')}): {row.get('claim')}. Fix and re-run prove-it, "
                    f"rule with fno inbox decide, or close with --force --reason."
                ),
            )

    # The gate fires ONLY on an explicit declaration. A plan that declares
    # neither close_probes nor expected_url_count closes exactly as it does
    # today - inferring "multi-wave" from `## Wave N` headings was rejected
    # because the common case (one .md == one PR == one node) uses waves as
    # internal structure and would false-positive identically to a half-ship,
    # parking every such node on autonomous /target. Coverage grows as
    # /blueprint stamps expected_url_count going forward; nothing retroactively
    # parks. A gate that only fires on an explicit promise cannot false-positive.
    # Condition B: run the declared outcome probes. Opt-in by construction (only
    # fires when the plan declared the field).
    if close_probes:
        runner = probe_runner or _close_probe_runner_shellout
        passed, detail = runner(plan_path_clean, cwd)
        if not passed:
            return PromiseVerdict(
                outcome="promise_unmet",
                reason=_promise_refusal_b(node_id, plan_display, detail),
            )

    # Condition C: the promised ship count vs. merged refs. Only the rare
    # multi-ship plan pays for gh I/O here; a 1-wave / 1-PR plan never reaches it.
    if isinstance(expected, int) and expected >= 2:
        refs = node_pr_refs(node)
        # `extra_refs` carries an explicit ship the close verb is recording but
        # has not yet persisted (e.g. `done --pr <new-final-pr>`): without it
        # the second merged PR is counted as missing and exit 6 deadlocks the
        # command out of ever writing it.
        if extra_refs:
            seen = {n for n, _ in refs}
            for num, url in extra_refs:
                if isinstance(num, int) and num not in seen:
                    refs.append((num, url))
                    seen.add(num)
        merged, failure = _count_merged_refs(
            refs, ceiling=expected, cwd=cwd, query=query
        )
        if merged < expected:
            # An unreachable ref is not a missing ship. Refusing on a gh outage
            # would tell the operator the plan under-shipped when gh was simply
            # down, and the merge gate already treats an outage as retryable
            # (exit 4) rather than a policy refusal.
            if failure and failure.retryable:
                # Unknown, not ok: the read was down, so the ship count is
                # unconfirmed in BOTH directions. Closing here stamped a
                # declared multi-ship node done on the strength of an outage.
                # The node stays open and the sweep retries when gh answers.
                # The remedy names the verb that can actually recover, which is
                # NOT always reconcile. When `extra_refs` carried an explicit
                # ship the close verb had not yet persisted, the refusal exits
                # before the write, so reconcile cannot see that ref at all: it
                # would re-count the STORED refs, find no failure, and answer
                # the permanent policy refusal (exit 6, "promised N; only M
                # merged") about a ship that is merged. Re-running the same
                # command is the path that both records the ref and closes.
                retry = (
                    "Re-run the same close command once GitHub answers; it "
                    "records the explicit --pr ref, which this refusal exited "
                    "before writing."
                    if extra_refs
                    else f"Retry with `fno backlog reconcile --node {node_id}` "
                    f"once GitHub answers."
                )
                return PromiseVerdict(
                    outcome="promise_unknown",
                    reason=(
                        f"Unknown: {node_id} could not confirm {expected} ships "
                        f"({merged} confirmed MERGED): {failure}\n"
                        f"  The read failed retryably; the node stays open. {retry}"
                    ),
                )
            if failure:
                return PromiseVerdict(
                    outcome="promise_unmet",
                    reason=(
                        f"Refused: {node_id} could not verify promised ships: "
                        f"{failure}\n  {failure.remedy_for(pr_number=0, repo=None)}"
                    ),
                )
            return PromiseVerdict(
                outcome="promise_unmet",
                reason=_promise_refusal_c(node_id, plan_display, expected, merged),
            )

    return PromiseVerdict(outcome="ok")


def _count_merged_refs(
    refs: list[tuple[int, Optional[str]]],
    *,
    ceiling: int,
    cwd: Optional[str] = None,
    query: Optional[Callable[..., PrMergeState]] = None,
) -> tuple[int, Optional[ReconcileError]]:
    """Count MERGED refs; return ``(merged, first typed failure)``.

    Mirrors :func:`resolve_merge_evidence`'s per-ref resolution so the two agree
    on what counts as merged. Stops early at ``ceiling``: once enough ships are
    confirmed to satisfy the promise, the remaining refs cannot change the
    verdict and are not queried. An unreachable ref is reported separately from
    a genuinely unmerged one: the caller must not read a gh outage as a short
    ship count.
    """
    query = query or query_pr_merge_state
    merged = 0
    failure: Optional[ReconcileError] = None
    repo: Optional[str] = None
    for pr_number, pr_url in refs:
        pr_repo = repo_slug_from_url(pr_url) or repo
        if repo is None:
            repo = pr_repo
        pr_cwd = cwd if pr_repo is None else None
        try:
            state = query(pr_number, repo=pr_repo, cwd=pr_cwd)
        except ReconcileError as exc:
            if failure is None or (failure.retryable and not exc.retryable):
                failure = ReconcileError(
                    f"PR #{pr_number}: {exc}",
                    kind=exc.kind,
                    remedy=exc.remedy_for(pr_number=pr_number, repo=pr_repo),
                )
            continue
        if state.state == "MERGED":
            merged += 1
            if merged >= ceiling:
                return merged, None
        elif state.state == "UNKNOWN":
            failure = ReconcileError(
                f"PR #{pr_number}: REST reader returned UNKNOWN state",
                kind="malformed",
            )
    return merged, failure


def _promise_refusal_b(node_id: str, plan_display: str, detail: str) -> str:
    return (
        f"Refused: {node_id} declared close_probes and at least one failed.\n"
        f"  plan: {plan_display}\n"
        f"  {detail}\n"
        f"\n"
        f"  Two legal exits:\n"
        f"    make every close_probe pass, then close; or\n"
        f"    close with --force --reason \"<why the unmet probe is acceptable>\""
    )


def _promise_refusal_c(node_id: str, plan_display: str, expected: int, merged: int) -> str:
    return (
        f"Refused: {node_id} promised {expected} ships; only {merged} merged.\n"
        f"  plan: {plan_display}\n"
        f"  promised ships: {expected}    merged refs: {merged}\n"
        f"\n"
        f"  Two legal exits:\n"
        f"    ship the rest, then close; or\n"
        f"    file the remainder (`fno backlog idea`) and close with\n"
        f"      --force --reason \"remaining ships filed as <id>\""
    )


def summarize_promise_held(
    held: "list[tuple[str, str, str]]", *, dry_run: bool = False
) -> str:
    """Render reconcile's held-open roll, split by WHY the node stayed open.

    ``held`` rows are ``(node_id, first_reason_line, outcome)``. The two
    outcomes need different operator sentences: an unmet promise is a policy
    refusal the operator resolves, an unknown is a read outage that resolves
    itself on the next sweep.
    """
    verb = "Holding" if dry_run else "Held"
    headlines = {
        "promise_unmet": "merged PR, unmet plan promise",
        "promise_unknown": "ship count unconfirmed, retryable read failure",
        "reopen_held": "deliberate reopen postdates the merge",
    }
    # Grouped by the outcomes PRESENT, not by a hard-coded pair. A closed
    # enumeration is the same shape the `satisfied` property exists to kill:
    # a fourth outcome would be dropped from the roll, and a sweep holding
    # only those nodes would print a blank line while holding them open.
    order = list(headlines) + sorted({o for _, _, o in held} - set(headlines))
    lines: list[str] = []
    for outcome in order:
        rows = [(nid, reason) for nid, reason, out in held if out == outcome]
        if not rows:
            continue
        headline = headlines.get(outcome, outcome)
        lines.append(f"{verb} {len(rows)} node(s) open ({headline}):")
        lines.extend(f"  {nid}: {reason}" for nid, reason in rows)
    return "\n".join(lines)


def _unharvested_deferred_carveouts(cwd: Optional[str]) -> list[dict]:
    """Unharvested ``deferred`` carve-outs on the node's project ledger.

    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    root = _carveout_ledger_root(cwd)
    try:
        from fno.carveout.core import read_carveouts

        return read_carveouts(root, kind="deferred")
    except Exception:
        return []


def _carveout_ledger_root(cwd: Optional[str]) -> Path:
    """The canonical root whose ``.fno/carveouts.jsonl`` owns the node's work.

    Resolved from ``cwd`` (the node's checkout) via ``git worktree list`` so a
    cross-project close reads the right project's ledger; falls back to the
    ambient canonical root, then to ``cwd``/cwd. Never raises.
    """
    if cwd:
        try:
            from fno.paths import resolve_canonical_worktree

            wt = resolve_canonical_worktree(Path(cwd))
            if wt is not None:
                return wt
        except Exception:
            pass
    try:
        from fno.carveout.core import resolve_carveout_root

        return resolve_carveout_root()
    except Exception:
        return Path(cwd) if cwd else Path.cwd()


def _promise_refusal_d(node_id: str, deferred: list[dict]) -> str:
    # Name each unharvested deferred carve-out by id + need (or description) so
    # the operator sees exactly what declared scope did not ship. Cap the list
    # so a flooded ledger stays readable.
    cap = 5
    rows = []
    for rec in deferred[:cap]:
        label = rec.get("need") or str(rec.get("description", ""))[:80]
        rows.append(f"    {rec.get('id', '?')}: {label}")
    more = (
        f"\n    ...and {len(deferred) - cap} more" if len(deferred) > cap else ""
    )
    return (
        f"Refused: {node_id} would close with {len(deferred)} unharvested "
        f"deferred carve-out(s) filed by this node's session.\n"
        f"  A `deferred` carve-out is declared scope that did not ship.\n"
        f"  carve-outs:\n" + "\n".join(rows) + f"{more}\n\n"
        f"  Two legal exits:\n"
        f"    harvest them into nodes (`fno backlog retro sweep-carveouts` previews;\n"
        f"      `--apply` files and consumes each), then close; or\n"
        f"    close with --force --reason \"deferred carve-out <id> filed as <node>\""
    )


def query_pr_merge_state(
    pr_number: int,
    *,
    repo: Optional[str] = None,
    cwd: Optional[str] = None,
    runner: Optional[Callable[..., subprocess.CompletedProcess]] = None,
    timeout_s: float = GH_QUERY_TIMEOUT_S,
    info_reader: Optional[Callable[..., tuple[Optional[dict], str]]] = None,
    files_reader: Optional[Callable[..., tuple[Optional[list[str]], str]]] = None,
    include_files: bool = False,
) -> PrMergeState:
    """Read merge state and complete changed-file evidence through REST.

    ``info_reader`` and ``files_reader`` are the production seams. ``runner``
    remains a compatibility seam for callers that inject ``subprocess.run``;
    it is adapted to the same in-process REST readers and never emits
    the old GraphQL PR-state argv.
    """
    from fno.pr import _rest
    from fno.pr._proc import Result

    def rest_runner(cmd, *, cwd=None, timeout=None):
        effective_timeout = timeout if timeout is not None else timeout_s
        try:
            if runner is None:
                return _rest.run(cmd, cwd=cwd, timeout=effective_timeout)
            result = runner(
                cmd,
                capture_output=True,
                text=True,
                check=False,
                timeout=effective_timeout,
                cwd=cwd,
            )
        except subprocess.TimeoutExpired:
            return Result(124, "", f"gh REST request timed out after {timeout_s}s")
        except OSError as exc:
            return Result(127, "", f"gh subprocess failed to launch: {exc}")
        return Result(
            int(getattr(result, "returncode", 1)),
            getattr(result, "stdout", "") or "",
            getattr(result, "stderr", "") or "",
        )

    info_reader = info_reader or (
        lambda number, *, repo=None, cwd=None: _rest.fetch_pr_info_rest(
            str(number), cwd=cwd, runner=rest_runner, repo=repo
        )
    )
    files_reader = files_reader or (
        lambda number, *, repo=None, cwd=None: _rest.fetch_pr_file_paths_rest(
            str(number), cwd=cwd, runner=rest_runner, repo=repo
        )
    )

    # Merged-PR facts never change: serve the permanent gh-cache row; injected readers bypass.
    cached = None
    if info_reader is None:
        from fno.rust_binary import VerbUnavailable, verb_call
        try:
            ask = {"op": "read", "kind": "merged", "slug": repo, "pr": pr_number}
            cached = verb_call("gh-cache", ask).get("row")
        except VerbUnavailable:
            pass
    row: dict = cached if isinstance(cached, dict) else {}
    info = row.get("info") if isinstance(row.get("info"), dict) else None
    files_ok = not include_files or isinstance(row.get("files"), list)
    if info is not None and isinstance(info.get("state"), str) and files_ok:
        number = info.get("pr") if isinstance(info.get("pr"), int) else pr_number
        files = list(row.get("files") or []) if include_files else []
        return PrMergeState(
            number=number, state=info["state"], url=info.get("url"),
            merged_at=info.get("merged_at"), merge_sha=info.get("merge_sha"),
            changed_files=files, files_truncated=False,
        )

    try:
        info, reason = info_reader(pr_number, repo=repo, cwd=cwd)
    except ReconcileError:
        raise
    except Exception as exc:  # noqa: BLE001 - reader failures become typed read errors
        log.exception("REST PR info reader failed for #%s repo=%s", pr_number, repo)
        kind = "availability" if type(exc).__name__ == "ToolMissing" else "reader_error"
        raise ReconcileError(str(exc), kind=kind) from exc
    if info is None:
        log.warning("REST PR info read failed for #%s repo=%s: %s", pr_number, repo, reason)
        failure = ReconcileError(str(reason or "REST PR info read failed"))
        failure.remedy = failure.remedy_for(pr_number=pr_number, repo=repo)
        raise failure
    if not isinstance(info, dict):
        raise ReconcileError(
            "REST PR info reader returned a value that is not an object",
            kind="malformed",
        )

    if "pr" not in info or "state" not in info:
        raise ReconcileError(
            "REST PR info reader omitted required PR number or state",
            kind="malformed",
        )
    state = info["state"]
    if state not in {"OPEN", "CLOSED", "MERGED"}:
        raise ReconcileError(
            f"REST PR info reader returned malformed state {state!r}",
            kind="malformed",
        )

    changed_files: list[str] = []
    if state == "MERGED" and include_files:
        try:
            file_paths, reason = files_reader(pr_number, repo=repo, cwd=cwd)
        except ReconcileError:
            raise
        except Exception as exc:  # noqa: BLE001 - reader failures become typed read errors
            log.exception("REST PR files reader failed for #%s repo=%s", pr_number, repo)
            kind = "availability" if type(exc).__name__ == "ToolMissing" else "reader_error"
            raise ReconcileError(str(exc), kind=kind) from exc
        if file_paths is None:
            log.warning("REST PR files read failed for #%s repo=%s: %s", pr_number, repo, reason)
            failure = ReconcileError(str(reason or "REST PR files read failed"))
            failure.remedy = failure.remedy_for(pr_number=pr_number, repo=repo)
            raise failure
        if not isinstance(file_paths, list) or not all(
            isinstance(path, str) and path for path in file_paths
        ):
            raise ReconcileError(
                "REST PR files reader returned malformed file paths",
                kind="malformed",
            )
        changed_files = list(file_paths)

    try:
        number = int(info.get("pr", pr_number))
    except (TypeError, ValueError) as exc:
        raise ReconcileError(
            "REST PR info reader returned a malformed PR number",
            kind="malformed",
        ) from exc

    if info_reader is None and state == "MERGED":
        from fno.rust_binary import VerbUnavailable, verb_call
        try:
            row = {"info": info, **({"files": changed_files} if include_files else {})}
            ask = {"op": "write", "kind": "merged", "slug": repo, "pr": pr_number, "row": row}
            verb_call("gh-cache", ask)
        except VerbUnavailable:
            pass
    return PrMergeState(
        number=number,
        state=state,
        url=info.get("url"),
        merged_at=info.get("merged_at"),
        merge_sha=info.get("merge_sha"),
        changed_files=changed_files,
        files_truncated=False,
    )


# Keep the production-reader identity stable when tests replace the module
# symbol. Reconciliation opts into file evidence only for this reader, while
# injected merge-state stubs keep their historical narrow signature.
_DEFAULT_QUERY_PR_MERGE_STATE = query_pr_merge_state


def clear_reopen_warning_if_child_matches(parent: dict, child: object) -> None:
    """Clear ``parent``'s ``reopen_warning`` when it names ``child``'s id.

    The child that reopened this already-done parent just closed again: the
    marker no longer describes a live state. Runs unconditionally on
    ``_cascade_close_parents``'s climb, whether or not the parent re-closes.
    """
    marker = parent.get("reopen_warning")
    child_id = child.get("id") if isinstance(child, dict) else None
    if isinstance(marker, dict) and marker.get("child") == child_id:
        parent.pop("reopen_warning", None)


def _parse_iso(value: object) -> Optional[datetime]:
    if not isinstance(value, str) or not value:
        return None
    try:
        dt = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    return dt if dt.tzinfo else dt.replace(tzinfo=timezone.utc)


def _reopen_outranks_child_closes(parent: dict, kids: list[dict]) -> bool:
    """True when a deliberate reopen postdates every child's close.

    Both close paths ask :func:`children_all_closed` - every child closed, at
    least one really shipped - and that predicate stays true forever once the
    last child merges. So a node reopened after its children finished was
    re-closed by the very next sweep:
    measured 2026-09-05, a reopen carrying a written reason was overridden by
    ``reconcile`` seven minutes later, and no retry could hold it. ``reopen``
    requires ``--reason`` precisely because a close is evidenced by a merged PR
    while a reopen is nothing but human judgment; an automatic sweep discarding
    that judgment without a word is the defect this guards.

    Keyed on the CHILDREN's closes rather than on "now", so it expires by
    itself. A reopen made BEFORE the last child landed is stale and the cascade
    still fires, because the parent genuinely became complete after that
    judgment was formed. Only a reopen postdating every child close is a
    statement about the finished set, and only that one holds.

    Ambiguity favours the human, deliberately and in both directions: an
    unreadable ``reopened_at`` protects, and so does an unreadable child
    ``completed_at``. Those stamps are written by one place in one format, so a
    value this cannot parse means something is already wrong, and re-closing on
    the strength of a timestamp we failed to read is the same silent override in
    a different coat. A parent carrying no ``reopened_at`` is unaffected, which
    is every node that was never reopened.
    """

    reopened_raw = parent.get("reopened_at")
    if not isinstance(reopened_raw, str) or not reopened_raw.strip():
        return False
    reopened = _parse_iso(reopened_raw)
    if reopened is None:
        return True
    for kid in kids:
        closed = _parse_iso(kid.get("completed_at"))
        if closed is not None and closed >= reopened:
            return False
    return True


def _reopen_outranks_merge(node: dict, merged_at: object) -> bool:
    """True when a deliberate reopen postdates the merge being closed on.
    Merge-keyed twin of :func:`_reopen_outranks_child_closes`; full contract:
    docs/architecture/backlog-graph-verb-contracts.md
    """

    reopened_raw = node.get("reopened_at")
    if not isinstance(reopened_raw, str) or not reopened_raw.strip():
        return False
    reopened = _parse_iso(reopened_raw)
    if reopened is None:
        return True
    merged = _parse_iso(merged_at) if isinstance(merged_at, str) else None
    return merged is None or reopened > merged


def reopen_held_reason(node: dict, pr_number: object) -> str:
    """Hold-row reason for a reopen that outranks a merge: the reopener's own words."""
    why = node.get("reopened_reason")
    why = why.strip() if isinstance(why, str) else ""
    return f"reopened after PR #{pr_number} merged: {why or 'no reopen reason recorded'}"


def _merge_postdates_reopen(
    node: dict,
    *,
    skip_pr: Optional[int],
    query: Callable[..., PrMergeState],
    cwd: Optional[str] = None,
) -> bool:
    """True when a ref OTHER than ``skip_pr`` merged after the node's reopen.
    Expiry half of :func:`_reopen_outranks_merge`; full contract:
    docs/architecture/backlog-graph-verb-contracts.md
    """

    reopened_raw = node.get("reopened_at")
    reopened = _parse_iso(reopened_raw) if isinstance(reopened_raw, str) else None
    if reopened is None:
        return False
    for number, url in node_pr_refs(node):
        if number == skip_pr:
            continue
        try:
            state = query(number, repo=repo_slug_from_url(url), cwd=cwd)
        except Exception:  # noqa: BLE001 - a read outage must not close on a guess
            return False
        if state.state != "MERGED" or not state.merged_at:
            continue
        merged = _parse_iso(state.merged_at)
        if merged is not None and merged > reopened:
            return True
    return False


def children_all_closed(parent: dict, kids: list[dict]) -> bool:
    """Every child is closed, and at least one really shipped.

    The one closure question both epic close paths ask
    (:func:`cascade_close_should_stop` and ``_strandable_epic_ids``). A child
    counts closed when :func:`fno.graph.statuses.is_terminal_entry` says so -
    done, superseded, or freshly stamped ``completed_at`` with its status
    still catching up - EXCEPT a child superseded BY THIS PARENT: the parent
    absorbed that child's work, so the parent's own liveness keeps it open.
    The shipped term keeps a parent whose every child was replaced elsewhere
    from closing as done with nothing built.
    """
    from fno.graph.statuses import is_terminal_entry

    pid = parent.get("id")
    if not kids:
        return False
    if any(not is_terminal_entry(k) or k.get("superseded_by") == pid for k in kids):
        return False
    return any(k.get("status") != "superseded" and not k.get("superseded_by") for k in kids)


def cascade_close_should_stop(parent: dict, kids: list[dict], child: object) -> bool:
    """One climb step of ``_cascade_close_parents``: clear a stale marker on
    ``parent`` first, then report whether the walk stops here (``parent``
    already done, a kid still open, or a deliberate reopen that outranks the
    sweep) rather than closing it and climbing on.
    """
    clear_reopen_warning_if_child_matches(parent, child)
    if parent.get("completed_at"):
        return True
    if _reopen_outranks_child_closes(parent, kids):
        return True  # reopened after its children finished -> the human call holds
    if parent.get("plan_path") and not (
        parent.get("pr_number") or parent.get("additional_prs")
    ):
        return True  # own plan work: deliverables unverified, the close false-fires
    return not children_all_closed(parent, kids)


def _owning_state_path(record: MergeDriftRecord) -> Optional[Path]:
    """Resolve the owning target-state.md from a record's ``cwd``.

    Returns None when the record carries no usable cwd (nothing to satisfy).
    The ``isinstance`` guard matters because ``cwd`` is copied raw from the graph
    node (``node.get("cwd")``) with no type check; a corrupted/hand-edited graph
    could carry a non-string cwd, and ``Path(non_str)`` raises TypeError. A raise
    here would abort the reconcile loop and strand later records, violating the
    best-effort contract - so a non-string cwd is treated as "no cwd".
    """
    if not isinstance(record.cwd, str) or not record.cwd:
        return None
    return Path(record.cwd) / ".fno" / "target-state.md"


def _read_state_frontmatter(state_path: Path) -> dict[str, Any]:
    """Parse the YAML frontmatter of a target-state.md into a dict.

    Local parse (mirrors ``worker.reconcile._read_state``) so this module does
    not couple to a private events helper. Returns {} on any parse problem.
    """
    import yaml  # local import: keeps the module's import cost off the hot scan path

    try:
        text = state_path.read_text(encoding="utf-8")
    except OSError:
        return {}
    if not text.startswith("---"):
        return {}
    rest = text[3:].lstrip("\n")
    end = rest.find("\n---")
    if end == -1:
        return {}
    try:
        return yaml.safe_load(rest[:end]) or {}
    except yaml.YAMLError:
        return {}


def emit_session_satisfied_for_record(
    record: MergeDriftRecord,
    *,
    reason: str = "reconcile_detected_merge",
) -> Optional[Path]:
    """Emit a ``session_satisfied{source:"pr_merge"}`` event for the target

    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    state_path = _owning_state_path(record)
    if state_path is None or not state_path.exists():
        return None

    fields = _read_state_frontmatter(state_path)
    # Only satisfy a session that is still live. A COMPLETE/BLOCKED/ABORTED
    # session needs no nudge; emitting would be noise.
    if str(fields.get("status") or "").strip() != "IN_PROGRESS":
        return None

    # Cross-check the resolved state file actually owns THIS node's PR before
    # nudging it. A cwd can be recycled: node A (merged, being reconciled here)
    # recorded a worktree that a live session is now using for an unrelated node
    # B. Without this guard we would emit a pr_merge session_satisfied bound to
    # B's session. It is not a false-complete (B's stop hook re-derives the
    # verified-merge ground truth against B's own pr_number and re-enforces its
    # gates), but it is a spurious cross-session signal. Skip unless the state
    # file's pr_number matches the merged PR we are reconciling.
    state_pr = fields.get("pr_number")
    try:
        if state_pr is None or int(state_pr) != int(record.pr_number):
            return None
    except (TypeError, ValueError):
        return None

    # The state file is the source of truth the stop hook compares against, so
    # read session_id from it (not record.session_id, which may be stale).
    session_id = str(fields.get("session_id") or "").strip()
    if not session_id or session_id == "null":
        return None

    try:
        gate_state_hash = hashlib.md5(state_path.read_bytes()).hexdigest()
    except OSError:
        return None

    events_path = state_path.parent / "events.jsonl"
    try:
        from fno import events

        event = events.session_satisfied(
            trigger="pr_merge",
            reason=reason,
            session_id=session_id,
            gate_state_hash=gate_state_hash,
            evidence_url=record.pr_url,
            source="backlog",
        )
        from fno.events.store_client import emit_envelope

        emit_envelope(event, events_path)
    except Exception as exc:  # best-effort: never abort the close on emit failure
        print(
            f"reconcile: session_satisfied emit failed for {record.node_id} "
            f"(session={session_id}): {type(exc).__name__}: {exc}; "
            f"merge close unaffected, defensive stop-hook probe is the backstop",
            file=sys.stderr,
        )
        return None
    return events_path


def emit_human_touch_for_record(record: MergeDriftRecord) -> Optional[Path]:
    """Emit ``human_touch{source:merge}`` for a node reconcile just closed (W4).

    An out-of-band merge (web merge button, bare ``gh pr merge``) is a human
    steering action no loop performed. Reconcile closes a node exactly once (a
    closed node is no longer scanned), so this fires once per node with no
    separate idempotence ledger (AC4-EDGE). Best-effort: a failure prints a
    diagnostic and never aborts the close.
    """
    try:
        from fno.events import _build

        event = _build(
            "human_touch",
            "backlog",
            {
                "graph_node_id": record.node_id,
                "source": "merge",
                "resolution": "ok",
            },
        )
        from fno.events.store_client import emit_envelope

        from fno.paths import project_events_json

        if isinstance(record.cwd, str) and record.cwd:
            events_path = Path(record.cwd) / ".fno" / "events.jsonl"
            emit_envelope(event, events_path)
            return events_path
        emit_envelope(event, project_events_json())
        return None
    except Exception as exc:  # best-effort: never abort the close on emit failure
        print(
            f"reconcile: human_touch emit failed for {record.node_id}: "
            f"{type(exc).__name__}: {exc}; merge close unaffected",
            file=sys.stderr,
        )
        return None


# Tier-1 gate_escape. A required review bot that never reviewed a PR
# which merged out-of-band is autonomy debt: the loop should have waited for /
# resolved that review, and the human merge past it is the escape. Kept as a
# module constant so the emit site and its tests name the same string.
_GATE_ESCAPE_REASON_DEADBOT = "dead-bot"
# an out-of-band merge of a PR nothing reviewed. Distinct from dead-bot
# (a required bot never reviewed) - this fires whether or not any bot was
# required, closing the gap where the dead-bot escape's early return on empty
# github_apps emitted nothing for the exact zero-coverage merges this node
# catches.
_GATE_ESCAPE_REASON_COVERAGE = "zero-coverage"


def _fetch_pr_review_logins(
    pr_number: int,
    *,
    repo: Optional[str] = None,
    cwd: Optional[str] = None,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
    timeout_s: float = GH_QUERY_TIMEOUT_S,
) -> set[str]:
    """Return the lowercased set of logins that reviewed a PR.

    Any review state (APPROVED / COMMENTED / CHANGES_REQUESTED) counts as "the
    bot reviewed" for the required-bot gate. Raises :class:`ReconcileError` on
    any gh failure so the caller fails OPEN (does not emit on uncertainty).
    """
    if _gh_executable() is None:
        raise ReconcileError("gh CLI not found on PATH")
    cmd = ["gh", "pr", "view", str(pr_number)]
    if repo:
        cmd += ["--repo", repo]
    cmd += ["--json", "reviews"]
    try:
        result = runner(
            cmd, capture_output=True, text=True, check=False, timeout=timeout_s, cwd=cwd
        )
    except subprocess.TimeoutExpired as exc:
        raise ReconcileError(
            f"gh pr view #{pr_number} reviews timed out after {timeout_s}s"
        ) from exc
    except OSError as exc:
        raise ReconcileError(f"gh subprocess failed to launch: {exc}") from exc
    if result.returncode != 0:
        raise ReconcileError(
            f"gh pr view #{pr_number} reviews failed (rc={result.returncode}): "
            f"{(result.stderr or '').strip()}"
        )
    try:
        row = json.loads(result.stdout or "{}")
    except json.JSONDecodeError as exc:
        raise ReconcileError(f"gh stdout was not JSON: {exc}") from exc
    if not isinstance(row, dict):
        raise ReconcileError("gh stdout was not a JSON object")
    logins: set[str] = set()
    for rev in row.get("reviews") or []:
        if not isinstance(rev, dict):
            continue
        author = rev.get("author")
        login = author.get("login") if isinstance(author, dict) else None
        if isinstance(login, str) and login:
            logins.add(login.lower())
    return logins


# Tier-1 reuses the shared gate_escape emit machinery: the canonical
# events path, the durable failure-log, and the final dedup+append all live in
# ONE place (fno.events.gate_escape) so Tier-1 (dead-bot) and Tier-2 (spawn-cap
# + the manual verb) never drift into parallel telemetry paths (Locked Decision
# 4). The `_canonical_events_path` alias is kept in THIS module's namespace so
# the resolve-before-fetch failure path (test_ac7_production_default) stays
# monkeypatchable.
from fno.events.gate_escape import (  # noqa: E402
    canonical_events_path as _canonical_events_path,
    emit_gate_escape as _emit_gate_escape,
    failure_log_path as gate_escape_failure_log_path,
    record_emit_failure as _record_gate_escape_emit_failure,
)


def _latest_review_coverage(pr_number: int, events_path: Path) -> Optional[dict]:
    """The latest ``review_coverage`` event data for ``pr_number``, or None.

    Fail-open: a missing/unreadable log or a parse error returns None (the
    caller under-reports rather than crashes).

    Reads the global log alongside ``events_path`` for the same reason the merge
    gate does: reconcile runs from canonical while the review that produced the
    event ran wherever the session stood, usually a worktree. Scanning one log
    left this detector reasoning about coverage it could not see.
    """
    try:
        from fno.pr._reviews import latest_review_coverage

        data = latest_review_coverage(
            pr_number,
            cwd=str(Path(events_path).parent.parent),
            project_events=Path(events_path),
        )
    except Exception:  # noqa: BLE001 - telemetry must never abort reconcile
        return None
    return data


def _emit_zero_coverage_escape(record: "MergeDriftRecord", events_path: Path) -> bool:
    """Emit a zero-coverage gate escape if the merged PR's ``review_coverage``
    event shows 0/unknown coverage. Returns True if emitted.

    Fires whether or not any bot was required, so a no-required-bots config that
    merged a PR nothing reviewed is still recorded as autonomy debt. A
    genuinely-reviewed PR (covered, count > 0) does not escape; no event at all
    (loop-check never ran) under-reports (fail-open). Returns True/False so the
    caller can short-circuit the dead-bot path when the headline gap is coverage.
    """
    cov = _latest_review_coverage(record.pr_number, events_path)
    if cov is None:
        return False
    if cov.get("coverage") == "covered":
        # The word decides, never the count: a budget spent on fail rounds
        # reads covered with passed_count 0, and the count conjunct here
        # reported that as "nothing reviewed this diff".
        return False  # genuinely reviewed; not an escape
    count = cov.get("reviewed_count", 0)
    count_str = str(count) if count is not None else "unknown"
    detail = (
        f"merged with {cov.get('coverage')} review coverage "
        f"({count_str} reviewed) - nothing reviewed this diff"
    )
    _emit_gate_escape(
        _GATE_ESCAPE_REASON_COVERAGE,
        pr=record.pr_number,
        node_id=record.node_id or None,
        detail=detail,
        events_path=events_path,
    )
    return True


def emit_gate_escape_for_record(
    record: MergeDriftRecord,
    *,
    required_bots: list[str],
    reviews_fetcher: Callable[..., set[str]] = _fetch_pr_review_logins,
    events_path: Optional[Path] = None,
) -> Optional[Path]:
    """Tier-1 auto-emit : a ``gate_escape{reason:dead-bot}`` when

    Full contract: docs/architecture/backlog-graph-verb-contracts.md
    """
    reason = _GATE_ESCAPE_REASON_DEADBOT
    resolved_events: Optional[Path] = events_path
    try:
        if record.pr_number <= 0:
            return None  # placeholder/unassigned PR number: nothing to escape

        # Resolve the events log NOW - before any escape check - so a fetch
        # failure still knows where to write the durable emit-failure counter
        # (AC7). Resolving a path is a non-mutating string computation; only the
        # conditional emit below writes.
        if resolved_events is None:
            resolved_events = _canonical_events_path(record.cwd)

        # zero-coverage escape fires whether or not any bot was required.
        # The dead-bot-only path below returned early when github_apps was empty,
        # so on a no-required-bots config it emitted nothing for the exact
        # zero-coverage merges this node exists to catch. Reads the
        # review_coverage event loop-check emitted; no event (loop-check never
        # ran, e.g. a pure out-of-band merge) under-reports (fail-open),
        # consistent with the dead-bot path's review-fetch-failure discipline.
        if _emit_zero_coverage_escape(record, resolved_events):
            return resolved_events

        wanted = [b for b in (required_bots or []) if isinstance(b, str) and b.strip()]
        if not wanted:
            return None  # AC2: nothing required, so nothing to escape (dead-bot)

        # On a review-fetch failure we CANNOT tell whether a required bot
        # reviewed, so we fail open (do not emit): a missed escape (under-report)
        # is the safe direction, and the failure is logged so retro surfaces the
        # blind spot rather than the metric silently reading low (AC7).
        repo = repo_slug_from_url(record.pr_url)
        reviewed = reviews_fetcher(record.pr_number, repo=repo, cwd=record.cwd)
        # Match a configured bot to its review login with the SAME semantics as
        # the ship gate (loopcheck.rs): strip a trailing ``[bot]`` suffix and do
        # a case-insensitive substring check, so ``github_apps: [gemini]`` counts
        # ``gemini-code-assist[bot]``'s review. Exact equality here would falsely
        # flag a bot that DID review under its gh ``[bot]`` login as dead-bot
        # (codex P2 on PR #232).
        from fno.pr_watch._discover import _reviewer_matches

        unmet = [b for b in wanted if not any(_reviewer_matches(lg, [b]) for lg in reviewed)]
        if not unmet:
            return None  # AC2b: every required bot reviewed; gate was met

        # Name the wedged bot(s) so retro's ranked output can point at the fix.
        detail = "required bot(s) never reviewed: " + ", ".join(sorted(unmet))
        # Delegate the dedup+append to the shared emit (dedup on (pr, reason),
        # fail-open + durable failure-log on an append error - AC4/AC5/AC7).
        return _emit_gate_escape(
            reason,
            pr=record.pr_number,
            node_id=record.node_id or None,
            detail=detail,
            events_path=resolved_events,
        )
    except Exception as exc:  # a fetch/resolve failure fails OPEN (AC5) + visible (AC7)
        fail_log = (
            gate_escape_failure_log_path(resolved_events)
            if resolved_events is not None
            else None
        )
        _record_gate_escape_emit_failure(fail_log, record.node_id, reason, exc)
        return None
