"""Backlog PR-merge drift detection for ``fno backlog reconcile``.

Pure logic split from I/O, mirroring ``fno.megatron.reconcile``. The
GitHub query is the only I/O dependency; tests stub ``query_pr_merge_state``
via the ``query`` parameter on :func:`scan_merge_drift`.

The completion ritual (stamp plan -> mark node ``done`` -> capture follow-ups)
runs automatically only when work flows through ``/target``'s ship gate or
``scripts/lib/pr-merge.sh``. A PR merged any other way (manual GitHub merge,
bare ``gh pr merge``) leaves the node open. This module detects that drift so
the CLI can close it mechanically, no memory required.
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from functools import lru_cache
from pathlib import Path
from typing import Any, Callable, Iterable, Literal, Optional, Union

# `gh pr view` default timeout; covers network hangs and stuck auth prompts.
GH_QUERY_TIMEOUT_S = 30.0

# reverse_map_unstamped fires one gh call per distinct repo in its scope
# (x-59a6 round-9 review fix). A --pr-number call now includes every open
# ref-less node graph-wide (needed so a stale ref-less node in another
# project still self-heals), so a multi-repo graph can fan out to several
# repos' worth of gh calls on a single merge. Same reasoning as cli.py's
# auto-discovery budget: a count alone still lets a degraded gh burn
# len(by_cwd)*GH_QUERY_TIMEOUT_S; a wall-clock budget bounds that too.
REVERSE_MAP_BUDGET_S = 60.0


@lru_cache(maxsize=1)
def _gh_executable() -> Optional[str]:
    """Resolve the gh binary path once per process.

    query_pr_merge_state runs per-PR inside scan_merge_drift's loop, so a
    bare shutil.which("gh") would re-scan PATH on every node. Cached here.
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


def pr_url_for_repo(
    pr: int,
    cwd: Optional[str] = None,
    *,
    runner: Callable[..., "tuple[int, str]"] = _run_slug_cmd,
) -> Optional[str]:
    """The canonical PR url for the checkout at ``cwd``, or None.

    A writer must resolve the url at least as capably as the reader resolves
    the slug, so this shares ``resolve_current_repo_slug``'s origin-then-gh
    chain: a url-less ``pr_number`` names no repo, and PR numbers collide
    across repos.

    An absent ``cwd`` resolves against the invocation checkout - the caller is
    standing in the repo it is stamping. A ``cwd`` that IS recorded but no
    longer exists returns None instead: that is positive evidence the node
    belongs to another repo, and `backlog done`/`backlog update` can name any node
    in the cross-project graph, so falling back would stamp the running repo's
    slug onto a foreign node.
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
    """Raised on gh failure or other I/O errors during a PR-state query."""


@dataclass
class PrMergeState:
    number: int
    # PrStateLiteral rather than the 3-value subset: query_pr_merge_state
    # falls back to "UNKNOWN" when gh omits the state field.
    state: PrStateLiteral
    url: Optional[str]
    merged_at: Optional[str]
    # mergeCommit.oid - the dedup key for post-merge-ritual auto-dispatch
    # (x-47be). Optional: absent on a non-merged PR or when gh omits it.
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
    # closing the node (Group 1 / ab-f7f8bc53). Both optional: a node may have
    # been intaken without a session (session_id null) or without a cwd.
    session_id: Optional[str] = None
    cwd: Optional[str] = None
    # mergeCommit.oid for the closed PR - the exactly-once dedup key for the
    # post-merge-ritual auto-dispatch (x-47be). None on a reverse-mapped record
    # (branch-name match has no SHA); the dispatcher falls back to a pr-number
    # key there.
    merge_sha: Optional[str] = None
    changed_files: list[str] = field(default_factory=list)
    files_truncated: bool = False

    @property
    def closeable(self) -> bool:
        return self.error is None and self.pr_state == "MERGED"


_GH_FILES_PAGE_SIZE = 100


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

    Returns positive receipts for predecessors that remain pending. A
    predecessor never becomes terminal from the relationship alone: every
    declared repo-relative surface must appear in the merged PR's changed-file
    evidence.

    ``evidence_complete=False`` says the file list is known to be short of the
    PR's real one. A surface missing from a truncated list is an absence with
    two explanations, so neither verdict is available: nothing verifies and the
    receipt names the truncation rather than blaming the surface.
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

    ``verify_pending_supersessions`` only ever runs while reconcile is CLOSING a
    successor. A successor that closed at any other moment - an earlier sweep, a
    hand-run ``fno backlog done``, or a supersede recorded against a node that
    had already shipped - never passes through that path. Its predecessors stay
    pending, pending reads as blocked, and nothing in the system ever revisits
    them: the only escape was ``unsupersede``.

    This finds those rows so the sweep can settle them against the evidence the
    successor already carries.
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


@dataclass
class PrRowBinding:
    node_id: str
    action: str  # filled_primary | appended_additional | already_bound | already_done


@dataclass
class PrRowBindResult:
    outcome: Literal["bound", "refused"]
    claimed_ids: list[str] = field(default_factory=list)
    bindings: list[PrRowBinding] = field(default_factory=list)
    refusal: Optional[str] = None

    @property
    def bound_ids(self) -> list[str]:
        return [
            binding.node_id
            for binding in self.bindings
            if binding.action in ("filled_primary", "appended_additional")
        ]


def bind_pr_rows(
    entries: list[dict],
    claimed_ids: list[str],
    *,
    pr_number: int,
    pr_url: Optional[str],
    repo: Optional[str] = None,
    owner: Optional[str] = None,
) -> PrRowBindResult:
    """Validate and bind every PR claim as one graph-owned mutation.

    Callers may run this against a copy for a dry-run or inside
    ``locked_mutate_graph`` for persistence. Validation completes before any
    row changes, so malformed, unknown, or cross-repository claims cannot
    leave a partial binding behind.
    """
    from fno.graph._constants import is_wellformed_node_id
    from fno.graph._intake import _find_node

    if not claimed_ids:
        return PrRowBindResult(outcome="refused", refusal="no closure claims to bind")

    our_repo = repo or repo_slug_from_url(pr_url)
    nodes: dict[str, dict] = {}
    for nid in claimed_ids:
        if not is_wellformed_node_id(nid):
            return PrRowBindResult(
                outcome="refused", claimed_ids=claimed_ids, refusal=f"malformed claim: {nid!r}"
            )
        node = _find_node(entries, nid)
        if node is None:
            return PrRowBindResult(
                outcome="refused", claimed_ids=claimed_ids, refusal=f"unknown node: {nid}"
            )
        existing_refs = node_pr_refs(node)
        if our_repo:
            for number, url in existing_refs:
                existing_repo = repo_slug_from_url(url)
                if existing_repo is None:
                    return PrRowBindResult(
                        outcome="refused",
                        claimed_ids=claimed_ids,
                        refusal=(
                            f"{nid} already carries a PR #{number} ref with no "
                            f"resolvable repo; this PR is {our_repo} - refusing an "
                            "unverifiable cross-repo claim"
                        ),
                    )
                if existing_repo.lower() != our_repo.lower():
                    return PrRowBindResult(
                        outcome="refused",
                        claimed_ids=claimed_ids,
                        refusal=(
                            f"{nid} already carries a {existing_repo} PR ref; "
                            f"this PR is {our_repo} - refusing a cross-repo claim"
                        ),
                    )
        elif existing_refs:
            return PrRowBindResult(
                outcome="refused",
                claimed_ids=claimed_ids,
                refusal=(
                    f"{nid} already carries a PR ref and this PR's repo is "
                    "unresolvable - refusing an unscoped claim"
                ),
            )
        nodes[nid] = node

    bindings: list[PrRowBinding] = []
    clean_owner = owner.strip() if isinstance(owner, str) else ""
    for nid in claimed_ids:
        node = nodes[nid]
        refs = node_pr_refs(node)
        if any(number == pr_number for number, _ in refs):
            action = "already_bound"
        elif not node_is_open(node):
            action = "already_done"
        elif not isinstance(node.get("pr_number"), int):
            node["pr_number"] = pr_number
            node["pr_url"] = pr_url
            action = "filled_primary"
        else:
            additional = list(node.get("additional_prs") or [])
            additional.append({"number": pr_number, "url": pr_url})
            node["additional_prs"] = additional
            action = "appended_additional"
        if clean_owner and node_is_open(node):
            node["locked_by"] = clean_owner
            node["session_id"] = clean_owner
        bindings.append(PrRowBinding(nid, action))

    return PrRowBindResult(outcome="bound", claimed_ids=claimed_ids, bindings=bindings)


def node_cwd_in_repo(entry: dict, our_root: str) -> bool:
    """Does ``entry``'s own ``cwd`` sit inside ``our_root`` (or is it missing)?

    A missing/empty/non-string cwd can't be proven NOT this repo's, and
    there is no cost to treating it as in-scope here: ``reverse_map_unstamped``
    (the only caller of this scope) independently skips a missing cwd
    unconditionally before it would ever fire a gh call, so this branch
    changes candidate-set membership only - never the observable close/skip
    outcome for a no-cwd node.

    Module-level (not nested inside a caller) precisely so this predicate is
    unit-testable on its own, independent of the reverse-map machinery that
    happens to make its no-cwd branch behaviorally inert today.
    """
    raw_cwd = entry.get("cwd")
    if not isinstance(raw_cwd, str) or not raw_cwd:
        return True
    our_root = os.path.normpath(our_root)
    our_root_prefix = our_root.rstrip(os.sep) + os.sep
    norm = os.path.normpath(os.path.expanduser(raw_cwd))
    return norm == our_root or norm.startswith(our_root_prefix)


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
    outage_error: Optional[str] = None
    open_pr_number: Optional[int] = None
    first_pr_number, first_pr_url = refs[0]
    repo = repo_slug_from_url(first_pr_url)

    for pr_number, pr_url in refs:
        pr_repo = repo_slug_from_url(pr_url) or repo
        pr_cwd = cwd if pr_repo is None else None

        try:
            pr_state = query(pr_number, repo=pr_repo, cwd=pr_cwd)
        except ReconcileError as exc:
            outage_error = str(exc)
            continue

        if pr_state.state == "MERGED":
            return MergeEvidence(outcome="merged", pr_url=pr_url)

        if pr_state.state == "OPEN":
            if open_pr_number is None:
                open_pr_number = pr_number
        else:
            refusal_reason = f"PR #{pr_number} state={pr_state.state} (not merged)"

    if open_pr_number is not None:
        # Carry any outage alongside: a ref we could not reach stays invisible
        # across every retry otherwise, since OPEN outranks it.
        return MergeEvidence(
            outcome="awaiting_merge",
            open_pr_number=open_pr_number,
            error=outage_error,
        )
    if outage_error:
        return MergeEvidence(outcome="outage", error=outage_error)
    return MergeEvidence(
        outcome="refused",
        reason=refusal_reason or f"PR #{first_pr_number}: no merged evidence",
    )


# The promise-gate refusal code. 3/4/5 are taken and load-bearing
# (REFUSAL_EXIT_CODES above; the loop runtime keys on 5), so this is the next
# free slot across graph/cli.py and done/cli.py. Do not renumber.
PROMISE_REFUSAL_EXIT_CODE = 6

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
    so a node can never close through a second, ungated path. Fails open
    (``outcome="ok"``) on an absent/unreadable/unparseable plan so a stale
    ``plan_path`` never wedges a close; the warning names the path.
    """

    outcome: Literal["ok", "promise_unmet"]
    reason: Optional[str] = None  # multi-line refusal text; None when ok
    # Why a fail-open happened (unreadable plan, unparseable frontmatter), naming
    # the path. Returned, not printed: the close verbs emit it on stderr, but a
    # raw stderr write here would pollute the `reconcile --json` stream (JSON on
    # stdout) and break the JSON-only contract of that path.
    warning: Optional[str] = None

    @property
    def exit_code(self) -> int:
        return PROMISE_REFUSAL_EXIT_CODE if self.outcome == "promise_unmet" else 0


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


def resolve_promise_evidence(
    node: dict,
    *,
    cwd: Optional[str] = None,
    query: Optional[Callable[..., PrMergeState]] = None,
    probe_runner: Optional[Callable[[str, Optional[str]], tuple[bool, str]]] = None,
    extra_refs: Optional[list[tuple[int, Optional[str]]]] = None,
    carveout_reader: Optional[Callable[[Optional[str]], "list[dict]"]] = None,
) -> PromiseVerdict:
    """Decide whether a node's plan promised work that has not all shipped.

    Fires ONLY on an explicit declaration - a plan that declares neither
    ``close_probes`` nor ``expected_url_count`` closes exactly as it does today.
    Inferring "multi-wave" from ``## Wave N`` headings was rejected: the common
    case (one .md == one PR == one node) uses waves as internal structure and
    would false-positive identically to a half-ship, parking every such node on
    autonomous /target. Coverage grows as /blueprint stamps ``expected_url_count``
    going forward, so nothing retroactively parks.

    Three conditions, first refusal wins. D reads the carve-out ledger and is
    independent of the plan; B and C read the plan at ``node["plan_path"]``:

      D. Unharvested deferred carve-outs. The project ledger
         (``.fno/carveouts.jsonl``) still carries a ``deferred`` carve-out -
         declared scope that did not ship. It must become a node (the retro
         harvest files and consumes it) or be force-overridden. ``oos-bug`` and
         ``backfill`` carve-outs do NOT block (genuine discovery, filed by the
         later harvest); both land in the same ledger and look identical, which
         is why a deferred item can merge away unnoticed (cv-99ebc0f3 on
         x-44cb). Checked first and independent of the plan so a close is held
         even when the plan is absent or unreadable. Scoped to the closing
         node's OWN rows via the ``node`` field stamped at capture time
         (``find_held_node`` proven ownership): a row filed by another node's
         session must not hold this close open (x-40be - one unrelated
         carve-out was blocking every close in the repo). Unattributed rows
         (legacy, ambient shell, harness without a session id) block nothing
         at close time; they stay visible via ``fno backlog carveout list`` and the
         retro sweep, which are the repo-wide backstop.
      B. Outcome probes. Any ``close_probes`` entry exits non-zero. Probes are
         delegated to ``fno-agents probe-run`` (the same runner the loop uses for
         ``done_probes``); a declared gate that cannot be evaluated fails closed.
      C. Ship count. ``expected_url_count: N`` (N >= 2) and fewer than N of the
         node's PR refs are MERGED. The right check for multi-repo / split
         deliveries.

    A gate that only fires on an explicit promise cannot false-positive, which
    is the reason the count is written at blueprint time rather than inferred
    afterward. Fails open on an absent/unreadable plan or unparseable
    frontmatter: a stale ``plan_path`` must not wedge a close, but the warning
    names the unreadable path so the gap is visible, not silent.
    """
    # Condition D (checked first; independent of the plan): an unharvested
    # `deferred` carve-out is declared scope that did not ship, and it blocks
    # the close until it becomes a node (harvest) or is force-overridden. Only
    # the closing node's OWN rows (``node`` field, stamped at capture time):
    # a repo-wide read held every close open on any unrelated row (x-40be).
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
            outcome="ok",
            warning=(
                f"promise gate could not read plan {plan_path_clean} ({exc}); "
                f"gate skipped for this close"
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
            outcome="ok",
            warning=(
                f"promise gate skipped {plan_path_clean}; plan frontmatter "
                f"would not parse ({exc})"
            ),
        )

    close_probes = frontmatter.get("close_probes")
    expected_raw = frontmatter.get("expected_url_count")
    expected = expected_raw if isinstance(expected_raw, int) else None
    node_id = node.get("id", "(unknown)")
    plan_display = node.get("plan_path", plan_path_clean)

    # The gate fires ONLY on an explicit declaration. A plan that declares
    # neither close_probes nor expected_url_count closes exactly as it does
    # today - inferring "multi-wave" from `## Wave N` headings was rejected
    # because the common case (one .md == one PR == one node) uses waves as
    # internal structure and would false-positive identically to a half-ship,
    # parking every such node on autonomous /target. Coverage grows as
    # /blueprint stamps expected_url_count going forward; nothing retroactively
    # parks. A gate that only fires on an explicit promise cannot false-positive.
    #
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
        merged, outage = _count_merged_refs(
            refs, ceiling=expected, cwd=cwd, query=query
        )
        if merged < expected:
            # An unreachable ref is not a missing ship. Refusing on a gh outage
            # would tell the operator the plan under-shipped when gh was simply
            # down, and the merge gate already treats an outage as retryable
            # (exit 4) rather than a policy refusal.
            if outage:
                return PromiseVerdict(
                    outcome="ok",
                    warning=(
                        f"promise gate could not confirm {expected} ships for "
                        f"{node_id}: {outage}; ship-count check skipped"
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
) -> tuple[int, Optional[str]]:
    """Count MERGED refs; return ``(merged, first_outage)``.

    Mirrors :func:`resolve_merge_evidence`'s per-ref resolution so the two agree
    on what counts as merged. Stops early at ``ceiling``: once enough ships are
    confirmed to satisfy the promise, the remaining refs cannot change the
    verdict and are not queried. An unreachable ref is reported separately from
    a genuinely unmerged one: the caller must not read a gh outage as a short
    ship count.
    """
    query = query or query_pr_merge_state
    merged = 0
    outage: Optional[str] = None
    repo: Optional[str] = None
    for pr_number, pr_url in refs:
        pr_repo = repo_slug_from_url(pr_url) or repo
        if repo is None:
            repo = pr_repo
        pr_cwd = cwd if pr_repo is None else None
        try:
            state = query(pr_number, repo=pr_repo, cwd=pr_cwd)
        except ReconcileError as exc:
            if outage is None:
                outage = f"PR #{pr_number}: {exc}"
            continue
        if state.state == "MERGED":
            merged += 1
            if merged >= ceiling:
                return merged, None
    return merged, outage


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
        f"  expected_url_count: {expected}    merged refs: {merged}\n"
        f"\n"
        f"  Two legal exits:\n"
        f"    ship the rest, then close; or\n"
        f"    file the remainder (`fno backlog idea`) and close with\n"
        f"      --force --reason \"remaining ships filed as <id>\""
    )


def _unharvested_deferred_carveouts(cwd: Optional[str]) -> list[dict]:
    """Unharvested ``deferred`` carve-outs on the node's project ledger.

    ``deferred`` blocks a close (declared scope did not ship); ``oos-bug`` and
    ``backfill`` do not (genuine discovery, filed by the later harvest). Both
    land in the same ``.fno/carveouts.jsonl``, which is why a deferred item can
    merge away unnoticed (cv-99ebc0f3 on x-44cb). The ledger is resolved from
    the NODE's project (``cwd``), not the ambient command repo: a cross-project
    close names a foreign node from this session, and reading the ambient
    ledger would both miss the foreign carve-out and let an unrelated local one
    block it. Falls back to the ambient canonical ledger when ``cwd`` is absent
    or unresolvable (the same-project close, the common case). Fails open on an
    unreadable ledger: a corrupt ledger must not wedge a close, and the retro
    harvest is the durable resolution path either way.
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
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
    timeout_s: float = GH_QUERY_TIMEOUT_S,
) -> PrMergeState:
    """Shell out to ``gh pr view <n> --json ...`` and parse the result.

    ``repo`` (``owner/repo``) scopes the lookup explicitly via ``--repo`` and
    is the authoritative way to avoid resolving a PR number against the wrong
    repository. ``cwd`` is a weaker fallback (gh infers owner/repo from the
    working directory's origin remote). Raises :class:`ReconcileError` on any
    gh failure (missing binary, auth, network, parse error, timeout).
    """
    if _gh_executable() is None:
        raise ReconcileError("gh CLI not found on PATH")

    cmd = ["gh", "pr", "view", str(pr_number)]
    if repo:
        cmd += ["--repo", repo]
    cmd += ["--json", "number,state,url,mergedAt,mergeCommit,files"]
    try:
        result = runner(
            cmd,
            capture_output=True,
            text=True,
            check=False,
            timeout=timeout_s,
            cwd=cwd,
        )
    except subprocess.TimeoutExpired as exc:
        raise ReconcileError(
            f"gh pr view #{pr_number} timed out after {timeout_s}s"
        ) from exc
    except OSError as exc:
        raise ReconcileError(f"gh subprocess failed to launch: {exc}") from exc

    if result.returncode != 0:
        raise ReconcileError(
            f"gh pr view #{pr_number} failed (rc={result.returncode}): "
            f"{(result.stderr or '').strip()}"
        )

    try:
        row = json.loads(result.stdout or "{}")
    except json.JSONDecodeError as exc:
        raise ReconcileError(f"gh stdout was not JSON: {exc}") from exc

    raw_files = row.get("files") or []
    changed_files = [
        item.get("path") if isinstance(item, dict) else item
        for item in raw_files
        if isinstance(item, str) or isinstance(item, dict)
    ]
    return PrMergeState(
        number=row.get("number", pr_number),
        state=row.get("state", "UNKNOWN"),
        url=row.get("url"),
        merged_at=row.get("mergedAt"),
        merge_sha=(row.get("mergeCommit") or {}).get("oid"),
        changed_files=[path for path in changed_files if isinstance(path, str) and path],
        # `gh pr view --json files` returns the first page only and does not
        # paginate, so a list at the cap is evidence of more, not of exactly
        # this many. Treat it as short and let the caller refuse to read an
        # absence out of it.
        files_truncated=len(raw_files) >= _GH_FILES_PAGE_SIZE,
    )


# W4 causal links: best-effort revert detection. A GitHub revert PR titles
# itself `Revert "..."` and auto-writes `Reverts owner/repo#N` in the body;
# a hand-written one usually keeps the git subject. Misses are a documented
# limitation with the manual `fno backlog update --reverted` fallback.
_REVERT_TITLE_RE = re.compile(r"^\s*Revert\b")
_REVERT_BODY_PR_RE = re.compile(r"\breverts\s+(?:([\w.-]+/[\w.-]+))?#(\d+)", re.IGNORECASE)


def fetch_recent_merged_prs(
    *,
    repo: Optional[str] = None,
    cwd: Optional[str] = None,
    limit: int = 30,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
    timeout_s: float = GH_QUERY_TIMEOUT_S,
) -> list[dict]:
    """Recently merged PRs (number/title/body) for revert detection.

    Returns ``[]`` when gh is absent (reconcile auto-fires on SessionStart;
    a gh-less machine must stay quiet). Raises :class:`ReconcileError` on a
    real gh failure so the caller can degrade with one warning.
    """
    if _gh_executable() is None:
        return []
    cmd = ["gh", "pr", "list", "--state", "merged", "--limit", str(limit)]
    if repo:
        cmd += ["--repo", repo]
    cmd += ["--json", "number,title,body,url"]
    try:
        result = runner(
            cmd, capture_output=True, text=True, check=False, timeout=timeout_s, cwd=cwd
        )
    except (subprocess.TimeoutExpired, OSError) as exc:
        raise ReconcileError(f"gh pr list failed: {exc}") from exc
    if result.returncode != 0:
        raise ReconcileError(
            f"gh pr list failed (rc={result.returncode}): {(result.stderr or '').strip()}"
        )
    try:
        rows = json.loads(result.stdout or "[]")
    except json.JSONDecodeError as exc:
        raise ReconcileError(f"gh stdout was not JSON: {exc}") from exc
    return rows if isinstance(rows, list) else []


def list_merged_pr_branches(
    *,
    cwd: str,
    limit: int = 100,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
    timeout_s: float = GH_QUERY_TIMEOUT_S,
) -> list[dict]:
    """Merged PRs (number/url/headRefName/mergedAt) for reverse-mapping.

    Run in ``cwd`` so gh resolves the repo from that dir's origin remote - the
    reverse map has no PR URL to scope from (that's the whole point: the node
    is unstamped). Returns ``[]`` when gh is absent. Raises
    :class:`ReconcileError` on a real gh failure so the caller degrades with
    one warning per repo.
    """
    if _gh_executable() is None:
        return []
    cmd = [
        "gh", "pr", "list", "--state", "merged", "--limit", str(limit),
        "--json", "number,url,headRefName,mergedAt",
    ]
    try:
        result = runner(
            cmd, capture_output=True, text=True, check=False, timeout=timeout_s, cwd=cwd
        )
    except (subprocess.TimeoutExpired, OSError) as exc:
        raise ReconcileError(f"gh pr list (merged) failed: {exc}") from exc
    if result.returncode != 0:
        raise ReconcileError(
            f"gh pr list (merged) failed (rc={result.returncode}): "
            f"{(result.stderr or '').strip()}"
        )
    try:
        rows = json.loads(result.stdout or "[]")
    except json.JSONDecodeError as exc:
        raise ReconcileError(f"gh stdout was not JSON: {exc}") from exc
    return rows if isinstance(rows, list) else []


def _branch_matches_node(head_ref: str, node_id: str) -> bool:
    """True when ``node_id`` is a full delimiter-bounded segment of ``head_ref``.

    ``branch_name()`` puts the whole node id in every dispatch branch as a
    ``/``- or ``-``-bounded segment (``feature/x-5b66``,
    ``target/some-slug-x-5b66``). A bare substring must NOT match: fixed-width
    hex ids make ``x-5b66`` a prefix of ``x-5b667``, so an unbounded match
    would close the wrong node.
    """
    if not head_ref or not node_id:
        return False
    return re.search(rf"(^|[/-]){re.escape(node_id)}([/-]|$)", head_ref) is not None


def _effective_reconcile_cwd(cwd: str, project: Optional[str]) -> str:
    """The dir reconcile should run a node's gh query / post-close routing in.

    Usually the node's recorded ``cwd`` (a worktree). When that worktree was
    archived (dir gone), fall back to the node's OWN project checkout so gh
    queries the right repo and post-close routing (advance/auto-continue) probes
    the campaign-arm marker under the real root, not a missing dir - but only
    when that root exists, else keep the original cwd so the existing degrade
    (per-node warning / node-cwd routing) is strictly unchanged.
    """
    if cwd and not os.path.isdir(cwd):
        from fno.graph._intake import project_root_from_settings

        root = project_root_from_settings(project)
        if root and os.path.isdir(root):
            return root
    return cwd


def _node_id_scope(node_id: Optional[Union[str, Iterable[str]]]) -> Optional[set[str]]:
    """Normalize a scan's ``node_id`` filter to a set, or None for unscoped.

    Accepts a single id (the common case) or an iterable of ids (x-59a6: a
    ``--pr-number`` call scopes the scan to every node this one PR actually
    touches - its own stamped ref plus every exact trailer claim - rather
    than either a single node or the whole graph.
    """
    if node_id is None:
        return None
    if isinstance(node_id, str):
        return {node_id}
    return set(node_id)


def reverse_map_unstamped(
    entries: list[dict],
    *,
    node_id: Optional[Union[str, Iterable[str]]] = None,
    list_merged: Optional[Callable[..., list[dict]]] = None,
) -> list[MergeDriftRecord]:
    """Close open nodes with NO PR refs by matching the id in a merged branch.

    A /target session that dies between ``gh pr create`` and the node<->PR
    stamp leaves an open node with no ``pr_number`` - invisible to the forward
    ``scan_merge_drift`` (which needs a ref to query). The branch convention
    (``branch_name()``) still carries the full node id, so one
    ``gh pr list --state merged`` per repo reverse-maps it. A unique headRef hit
    synthesizes the same MergeDriftRecord the stamped path emits (so the
    existing close path applies unchanged); an ambiguous hit (two merged PRs
    for one id) emits an ``error`` record naming both, never a guess.

    ``list_merged`` is injected in tests to avoid shelling to gh.
    """
    if list_merged is None:
        list_merged = list_merged_pr_branches
    _scope = _node_id_scope(node_id)

    # Open, ref-less, cwd-resolvable candidates grouped by repo dir so we make
    # ONE gh call per repo, not per node.
    # ponytail: group by cwd string; two worktrees of one repo -> two identical
    # gh calls. Collapse to git-common-dir only if that ever shows on a profile.
    by_cwd: dict[str, list[dict]] = {}
    skipped_dead_cwd: list[str] = []
    for node in entries:
        nid = node.get("id")
        if not isinstance(nid, str):
            continue
        if _scope is not None and nid not in _scope:
            continue
        if not node_is_open(node):
            continue
        if node_pr_refs(node):
            continue
        cwd = node.get("cwd")
        # str-only: a non-string cwd (corrupt graph) would become a bad dict key
        # here and a TypeError at the subprocess cwd= below.
        if not isinstance(cwd, str) or not cwd:
            continue
        # An archived-worktree cwd would make gh raise Errno 2; substitute the
        # node's project root when it's gone (also collapses same-project gone
        # worktrees to one gh call).
        cwd = _effective_reconcile_cwd(cwd, node.get("project"))
        # Original dead AND project-root fallback unresolvable: this ref-less node
        # cannot be reverse-mapped at all - handing the missing dir to
        # subprocess(cwd=) raises Errno 2, one hard failure per node on EVERY
        # reconcile (SessionStart + every merge). Skip it and surface ONE
        # aggregated advisory below instead of that permanent per-node spam.
        if not os.path.isdir(cwd):
            skipped_dead_cwd.append(nid)
            continue
        by_cwd.setdefault(cwd, []).append(node)

    if skipped_dead_cwd:
        # Name EVERY skipped id (not a capped subset): the id is the only handle
        # an operator has to heal a genuinely-merged-but-archived node via
        # `fno backlog update`, so a truncated list would leave the tail
        # un-healable - the exact visibility the aggregated advisory exists to
        # preserve. One line, however many ids; the count is realistically small.
        print(
            f"reverse-map: skipped {len(skipped_dead_cwd)} ref-less node(s) with "
            f"missing cwd: {' '.join(skipped_dead_cwd)} "
            f"(heal with: fno backlog update <id> --project <p> --cwd <path>)",
            file=sys.stderr,
        )

    records: list[MergeDriftRecord] = []
    _deadline = time.monotonic() + REVERSE_MAP_BUDGET_S
    _cwds = list(by_cwd.items())
    for _i, (cwd, nodes) in enumerate(_cwds):
        if time.monotonic() >= _deadline:
            _deferred = [n["id"] for _c, ns in _cwds[_i:] for n in ns]
            print(
                f"reverse-map: stopped at its {REVERSE_MAP_BUDGET_S:.0f}s budget "
                f"(gh is slow or degraded); deferred {len(_deferred)} node(s) to a "
                f"later sweep: {' '.join(_deferred)}",
                file=sys.stderr,
            )
            break
        try:
            merged = list_merged(cwd=cwd)
        except ReconcileError as exc:
            for node in nodes:
                records.append(
                    MergeDriftRecord(
                        node_id=node["id"], plan_path=node.get("plan_path"),
                        pr_number=0, pr_url=None, pr_state="UNKNOWN", merged_at=None,
                        error=f"reverse-map gh query failed: {exc}",
                        session_id=node.get("session_id"), cwd=cwd,
                    )
                )
            continue

        for node in nodes:
            nid = node["id"]
            hits = [
                row for row in merged
                if isinstance(row, dict)
                and _branch_matches_node(str(row.get("headRefName") or ""), nid)
            ]
            if not hits:
                continue
            if len({row.get("number") for row in hits}) > 1:
                nums = ", ".join(f"#{r.get('number')}" for r in hits)
                records.append(
                    MergeDriftRecord(
                        node_id=nid, plan_path=node.get("plan_path"),
                        pr_number=0, pr_url=None, pr_state="UNKNOWN", merged_at=None,
                        error=f"reverse-map ambiguous: {nums} both match branch id {nid}",
                        session_id=node.get("session_id"), cwd=cwd,
                    )
                )
                continue
            row = hits[0]
            records.append(
                MergeDriftRecord(
                    node_id=nid, plan_path=node.get("plan_path"),
                    pr_number=int(row.get("number") or 0), pr_url=row.get("url"),
                    pr_state="MERGED", merged_at=row.get("mergedAt"),
                    session_id=node.get("session_id"), cwd=cwd,
                )
            )
    return records


def detect_reverted_nodes(
    merged_prs: list[dict], entries: list[dict]
) -> list[tuple[str, int]]:
    """(node_id, revert_pr_number) pairs to stamp ``reverted: true``.

    A merged PR whose title starts with ``Revert`` and whose body references
    a PR number carried by a not-yet-reverted graph node names that node's
    ship as reverted. Pure (no I/O) so tests need no gh.

    Matching is REPO-SCOPED: the graph is global across projects, so bare PR
    numbers collide. A candidate node must carry a ``pr_url`` in the SAME
    repo as the revert PR's own ``url``, a body qualifier
    (``Reverts other/repo#N``) must match that repo, and an ambiguous match
    (two same-repo nodes on one number) stamps nothing - the same
    ambiguity-resolves-to-nothing rule as the W1 backfill.
    """
    # pr_number -> [(entry, that ref's repo slug)]; refs without a parseable
    # pr_url are indexed with slug None and never match (conservative).
    by_pr: dict[int, list[tuple[dict, Optional[str]]]] = {}
    for e in entries:
        for num, url in node_pr_refs(e):
            by_pr.setdefault(num, []).append((e, repo_slug_from_url(url)))

    out: list[tuple[str, int]] = []
    seen: set[str] = set()
    for pr in merged_prs:
        number = pr.get("number")
        if not isinstance(number, int) or number <= 0:
            continue
        if not _REVERT_TITLE_RE.match(str(pr.get("title") or "")):
            continue
        revert_slug = repo_slug_from_url(pr.get("url"))
        if revert_slug is None:
            # No repo context for the revert PR itself: refuse to match a
            # bare number against the multi-project graph.
            continue
        for m in _REVERT_BODY_PR_RE.finditer(str(pr.get("body") or "")):
            qualifier, target = m.group(1), int(m.group(2))
            if qualifier and qualifier.lower() != revert_slug.lower():
                continue  # explicit cross-repo reference: not this repo's PR
            matches = [
                e
                for e, slug in by_pr.get(target, [])
                if slug is not None
                and slug.lower() == revert_slug.lower()
                and not e.get("reverted")
            ]
            hit_ids = {e.get("id") for e in matches}
            if len(hit_ids) != 1:
                continue  # zero or ambiguous: stamp nothing
            nid = hit_ids.pop()
            if isinstance(nid, str) and nid not in seen:
                seen.add(nid)
                out.append((nid, number))
    return out


def scan_merge_drift(
    entries: list[dict],
    *,
    query: Optional[Callable[..., PrMergeState]] = None,
    node_id: Optional[Union[str, Iterable[str]]] = None,
    list_merged: Optional[Callable[..., list[dict]]] = None,
) -> list[MergeDriftRecord]:
    """Find open nodes whose PR has merged outside the ship gate.

    Returns one record per open node that resolves to a MERGED PR, plus
    records flagged with ``error`` for nodes whose PR state could not be
    resolved. Nodes whose PRs are all still OPEN (or closed-unmerged) yield no
    record - they are not drift. ``node_id`` restricts the scan to a single
    node (a str) or a set of nodes (an iterable of str) - e.g. every node one
    specific PR's exact trailer names, so a ``--pr-number`` call scans only
    what that PR could possibly touch instead of the whole graph (x-59a6).
    Tests inject a ``query`` stub to avoid shelling out to gh.

    A second pass (``reverse_map_unstamped``) covers open nodes with NO PR ref
    at all - a session that died before the node<->PR stamp - by matching the
    node id against merged branch names. ``list_merged`` is injected in tests.
    """
    if query is None:
        query = query_pr_merge_state
    _scope = _node_id_scope(node_id)

    records: list[MergeDriftRecord] = []

    for node in entries:
        nid = node.get("id")
        if not isinstance(nid, str):
            continue
        if _scope is not None and nid not in _scope:
            continue
        if not node_is_open(node):
            continue

        refs = node_pr_refs(node)
        if not refs:
            continue

        # Same dead-cwd defect as the reverse path: subprocess(cwd=<missing>)
        # raises Errno 2 even when --repo was parsed from pr_url and the cwd is
        # irrelevant. Resolve through the project-root fallback; if still gone,
        # degrade to None so a repo-scoped query still succeeds via --repo and a
        # repo-less node lands on the explicit `no repo context` error, not Errno 2.
        cwd = node.get("cwd")
        if isinstance(cwd, str) and cwd:
            cwd = _effective_reconcile_cwd(cwd, node.get("project"))
            if not os.path.isdir(cwd):
                cwd = None
        else:
            cwd = None
        merged: Optional[PrMergeState] = None
        first_error: Optional[str] = None

        for number, url in refs:
            # Prefer an explicit repo parsed from the PR URL so we never
            # resolve a PR number against the wrong repository. Fall back to
            # the node's cwd only when no URL is available. With neither, we
            # cannot safely identify the repo: record a failure rather than
            # risk closing a node off a same-numbered PR elsewhere.
            repo = repo_slug_from_url(url)
            if repo is None and not cwd:
                if first_error is None:
                    first_error = (
                        f"PR #{number}: no repo context (pr_url unparseable and "
                        f"cwd unset); refusing to query to avoid a wrong-repo match"
                    )
                continue
            try:
                state = query(number, repo=repo, cwd=cwd)
            except ReconcileError as exc:
                if first_error is None:
                    first_error = str(exc)
                continue
            if state.state == "MERGED":
                merged = state
                break

        if merged is not None:
            records.append(
                MergeDriftRecord(
                    node_id=nid,
                    plan_path=node.get("plan_path"),
                    pr_number=merged.number,
                    pr_url=merged.url,
                    pr_state="MERGED",
                    merged_at=merged.merged_at,
                    session_id=node.get("session_id"),
                    cwd=cwd,
                    merge_sha=merged.merge_sha,
                    changed_files=list(merged.changed_files),
                    files_truncated=merged.files_truncated,
                )
            )
        elif first_error is not None:
            # Could not resolve any PR for this open node. Surface it so the
            # caller can report a query failure rather than silently dropping.
            number, url = refs[0]
            records.append(
                MergeDriftRecord(
                    node_id=nid,
                    plan_path=node.get("plan_path"),
                    pr_number=number,
                    pr_url=url,
                    pr_state="UNKNOWN",
                    merged_at=None,
                    error=first_error,
                    session_id=node.get("session_id"),
                    cwd=cwd,
                )
            )

    records.extend(
        reverse_map_unstamped(entries, node_id=node_id, list_merged=list_merged)
    )

    return records


def write_retro_sentinel(record: MergeDriftRecord, *, sentinel_dir: Path) -> Path:
    """Drop a per-node retro sentinel naming a node closed by reconcile.

    The sentinel hands the judgment half (follow-up capture via inbox/triage)
    to a later session's LLM/human pass. Reconcile must NOT auto-create inbox
    lines or backlog nodes - that stays explicit. Overwrites an existing
    sentinel for the same node (idempotent).

    Only ever called with a closeable (resolved-merged) record; the assertion
    catches misuse (e.g. handing it an error record) in tests and dev.
    """
    assert record.closeable, f"refusing to write sentinel for non-closeable {record.node_id}"
    sentinel_dir.mkdir(parents=True, exist_ok=True)
    path = sentinel_dir / f"{record.node_id}.json"
    payload = {
        "node_id": record.node_id,
        "pr_number": record.pr_number,
        "pr_url": record.pr_url,
        "merged_at": record.merged_at,
        "plan_path": record.plan_path,
        "closed_by": "backlog-reconcile",
        "closed_at": datetime.now(timezone.utc).isoformat(),
    }
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    return path


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
    session that owns a merged-and-now-closed node (Group 1 / ab-f7f8bc53).

    Today only an in-gate merge through ``scripts/lib/pr-merge.sh`` emits this
    signal, so an out-of-band merge (web button, bare ``gh pr merge``) leaves the
    owning session hot and the stop hook hard re-blocks it. After reconcile
    closes the drifted node, this hands the same auto-complete signal to the
    owning session.

    The event binds to that session via ``session_id`` + ``gate_state_hash`` (the
    md5 of the owning target-state.md at emit time), matching the stop hook's
    staleness check (``check_session_satisfied``). The defensive stop-hook probe
    (Task 1.2) is the backstop for when this emit is stale or never lands.

    Best-effort and non-fatal: returns the events.jsonl path on a successful
    emit, or None when there is nothing to satisfy (no cwd, no live state file,
    already-COMPLETE session, missing session_id) or any failure. A failure here
    must never abort the reconcile close.
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
        events.append_event(event, events_path)
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
        from fno.events import _build, append_event

        event = _build(
            "human_touch",
            "backlog",
            {
                "graph_node_id": record.node_id,
                "source": "merge",
                "resolution": "ok",
            },
        )
        if isinstance(record.cwd, str) and record.cwd:
            events_path = Path(record.cwd) / ".fno" / "events.jsonl"
            append_event(event, events_path=events_path)
            return events_path
        append_event(event)
        return None
    except Exception as exc:  # best-effort: never abort the close on emit failure
        print(
            f"reconcile: human_touch emit failed for {record.node_id}: "
            f"{type(exc).__name__}: {exc}; merge close unaffected",
            file=sys.stderr,
        )
        return None


# Tier-1 gate_escape (x-f894). A required review bot that never reviewed a PR
# which merged out-of-band is autonomy debt: the loop should have waited for /
# resolved that review, and the human merge past it is the escape. Kept as a
# module constant so the emit site and its tests name the same string.
_GATE_ESCAPE_REASON_DEADBOT = "dead-bot"
# x-0eaf: an out-of-band merge of a PR nothing reviewed. Distinct from dead-bot
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


# Tier-1 reuses the shared gate_escape emit machinery (x-91b5): the canonical
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
    caller under-reports rather than crashes). (x-0eaf)

    Reads the global log alongside ``events_path`` for the same reason the merge
    gate does: reconcile runs from canonical while the review that produced the
    event ran wherever the session stood, usually a worktree. Scanning one log
    left this detector reasoning about coverage it could not see (x-f43c).
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
    merged a PR nothing reviewed is still recorded as autonomy debt (x-0eaf). A
    genuinely-reviewed PR (covered, count > 0) does not escape; no event at all
    (loop-check never ran) under-reports (fail-open). Returns True/False so the
    caller can short-circuit the dead-bot path when the headline gap is coverage.
    """
    cov = _latest_review_coverage(record.pr_number, events_path)
    if cov is None:
        return False
    if cov.get("coverage") == "covered":
        try:
            if int(cov.get("reviewed_count") or 0) > 0:
                return False  # genuinely reviewed; not an escape
        except (TypeError, ValueError):
            pass
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
    """Tier-1 auto-emit (x-f894): a ``gate_escape{reason:dead-bot}`` when
    reconcile closes an out-of-band-merged node whose required review bot never
    reviewed.

    Boundary (the #222 rule - the load-bearing correctness surface):
      - required_bots empty          -> NOT an escape: a no-required-bots repo
                                        self-merging a green PR is normal (AC2).
      - every required bot reviewed  -> NOT an escape: the gate was met; only
                                        the merge happened out of band (AC2b).
      - some required bot never reviewed -> escape: the loop should have waited
                                        for / resolved that review (AC1).

    Lands in the CANONICAL events log (``events_path`` overrides for tests) so a
    closed node's telemetry outlives its worktree and retro aggregates one
    coherent log. Dedup on (pr, reason) (AC4). Telemetry fails OPEN: any failure
    logs a durable emit-failure line beside the events log (AC7) and returns
    None, never raising - the emit must never abort the reconcile close (AC5).
    Returns the events.jsonl path on a successful emit.
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

        # x-0eaf: zero-coverage escape fires whether or not any bot was required.
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
