"""The PR-body closure line: parse, render, and bind PR-to-node closure.

A merged PR's body may name several backlog nodes, but only the ONE node
stamped into `.fno/target-state.md` at creation ever gets its `pr_number`
written to the graph - every other named node stays open forever, because
the forward scan in `_reconcile.scan_merge_drift` needs a PR ref to query and
the reverse branch-name map only carries the primary node's id.

Free-text mentions ("this also fixes x-aaaa", "blocked by x-bbbb") are
measurement-only (see `scripts/metrics/pr-node-closure-audit.py`) and must
NEVER become a closure claim - a dependency note or a follow-up filing reads
identically to a close claim to a prose scanner. The exact line is the only
runtime-recognized closure grammar, so a claim is either the literal line or
it does not exist.

The LINE FORMAT lives in one leg: the Rust parser (`crates/fno-agents/src/
king_board/pr_closure.rs`), exposed as `fno-agents pr closure parse|render`.
`parse_closure_trailer` and `render_closure_trailer` are thin forwarders to
that verb, so the Python and Rust readers can never disagree about what a
body claims. The keyword sits at the start of a line, case-insensitive, with
or without a colon; every token after it is a well-formed node id, split by
commas and/or spaces; one malformed token makes the line prose and it claims
nothing; the LAST well-formed line wins. Writers emit only `Fixes`; readers
also accept the retired `Backlog-Closure:` spelling while open PRs carry it.
"""
from __future__ import annotations

import copy
import re
import subprocess
from dataclasses import dataclass, field
from typing import Callable, Iterable, Optional

from fno.graph._constants import NODE_ID_BODY, is_wellformed_node_id


class ClosureBinaryError(RuntimeError):
    """The Rust closure leg failed or is missing; callers stop loudly rather
    than read a claim with a broken parser."""


def closure_call(args: list[str], payload: Optional[str]) -> str:
    """One call to `fno-agents pr closure <mode>`. `payload` rides stdin when
    given (parse); otherwise the ids come as argv (render). Module-level so
    tests can pin the leg without a binary."""
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        raise ClosureBinaryError(
            "fno-agents binary not found; the closure-line parser is the Rust "
            "leg (fno-agents pr closure). Reinstall fno, run `fno doctor update "
            "--rust`, or set FNO_AGENTS_BIN."
        )
    proc = subprocess.run(
        [str(binary), "pr", "closure", *args],
        input=payload,
        text=True,
        capture_output=True,
        check=False,
        timeout=30,
    )
    if proc.returncode != 0:
        raise ClosureBinaryError(
            f"pr closure {args[0] if args else '?'} failed "
            f"(rc={proc.returncode}): {(proc.stderr or '').strip()}"
        )
    return proc.stdout


def parse_closure_trailer(body: str) -> list[str]:
    """Well-formed node ids named on the LAST closure line of ``body``.

    A thin forwarder to the Rust leg (`fno-agents pr closure parse`); the
    grammar and its edge cases live there and in the shared corpus fixture
    (`tests/fixtures/pr-closure-cases.json`). Raises ``ClosureBinaryError``
    when the leg is missing or fails.
    """
    if not isinstance(body, str) or not body:
        return []
    import json

    out = closure_call(["parse"], body).strip()
    return json.loads(out) if out else []


def render_closure_trailer(node_ids: list[str]) -> str:
    """The one place a trailer LINE is built, so parse<->render round-trips.

    A thin forwarder to the Rust leg (`fno-agents pr closure render`); the
    Rust renderer drops malformed/duplicate ids and returns "" (no line) when
    nothing well-formed remains, so a caller can safely append the result to
    a body unconditionally. Emits only the ``Fixes`` spelling.
    """
    return closure_call(["render", *[n for n in node_ids]], None).strip()


def contained_descendant_ids(entries: list[dict], node_id: str) -> list[str]:
    """Every node whose ``contained_in`` points at ``node_id``, in graph order.

    These are units that ship INSIDE the same delivery (convention),
    so they belong in the same trailer as the primary target without an
    operator having to name them by hand.
    """
    return [
        e["id"]
        for e in entries
        if isinstance(e, dict)
        and isinstance(e.get("id"), str)
        and e.get("contained_in") == node_id
    ]


def render_pr_closure_trailer(
    entries: list[dict], node_id: str, *, extra_ids: Optional[list[str]] = None
) -> str:
    """The trailer for a PR built from ``node_id``: itself, its contained_in
    descendants, then any genuine additional delivery the caller names.
    """
    ids: list[str] = []
    if is_wellformed_node_id(node_id):
        ids.append(node_id)
        ids.extend(contained_descendant_ids(entries, node_id))
    for extra in extra_ids or []:
        if is_wellformed_node_id(extra):
            ids.append(extra)
    return render_closure_trailer(ids)


# ---------------------------------------------------------------------------
# Produce: the trailer a PR-creation path owes its own branch.
# ---------------------------------------------------------------------------

# Delimiter-bounded candidates from a head ref, the producer half of the set
# `scripts/ci/check-pr-node-closure.sh` demands. Non-overlapping left-to-right
# scanning is what makes the two agree on a ref like "feature/x-cccc-1234":
# once "x-cccc" is consumed the scan resumes at "-1234", which is not
# letter-led, so the bogus "cdef-1234" candidate the gate's skip-both-segments
# step exists to prevent is never produced on this side either.
_BRANCH_NODE_ID_RE = re.compile(rf"(?:^|[/-])({NODE_ID_BODY})(?=$|[/-])")


def branch_node_ids(head_ref: str) -> list[str]:
    """Well-formed node ids named as delimiter-bounded segments of ``head_ref``.

    Order-preserved, deduplicated. A bare substring never counts - fixed-width
    hex makes ```` a prefix of ``x-5b667`` - which is the same rule
    ``_branch_matches_node`` enforces on the reconcile side.
    """
    if not isinstance(head_ref, str) or not head_ref:
        return []
    ids: list[str] = []
    seen: set[str] = set()
    for match in _BRANCH_NODE_ID_RE.finditer(head_ref):
        candidate = match.group(1)
        if candidate not in seen:
            seen.add(candidate)
            ids.append(candidate)
    return ids


def known_node_ids() -> frozenset[str]:
    """Every id the graph actually carries; raises when the graph cannot be read.

    A read failure propagates, it never reads as empty. Measured 2026-09-16:
    empty looked safe because a trailer-less PR reds the CI gate loudly, but
    three PRs went red with no named cause and the dead reader answered exactly
    like a missing node. Now the exception stops the ``gh pr create`` path and
    names the read. Empty is reserved for the one safe case: an external
    tracker backend, where graph.json is not the delivery record and nothing is
    claimed.
    """
    from fno.graph import api as graph_api
    from fno.paths import graph_json
    from fno.tracker import active_backend_name

    if active_backend_name() != "graph":
        # graph.json is not the delivery record of truth under an external
        # tracker, which is the same posture `fno do pr closure-trailer` takes
        # there. Nothing to verify against, so nothing is claimed.
        return frozenset()
    return frozenset(
        e["id"]
        for e in (
            n.model_dump(by_alias=True)
            for n in graph_api.nodes(include_archived=True, path=graph_json()).nodes
        )
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    )


class BranchResolutionError(Exception):
    """Bare-verb branch resolution could not find exactly one real node; the message names why."""


def _read_current_branch(
    *,
    cwd: Optional[str] = None,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
) -> str:
    """The checked-out branch name; "" when HEAD is detached.

    The one branch read both consuming producers share (the bare-verb
    resolver and the created-PR binder), so the two can never drift.
    """
    try:
        proc = runner(
            ["git", "branch", "--show-current"],
            cwd=cwd,
            capture_output=True,
            text=True,
            check=False,
            timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise BranchResolutionError(f"branch lookup failed: {exc}") from exc
    if proc.returncode != 0:
        raise BranchResolutionError(
            f"branch lookup failed: {(proc.stderr or '').strip()}"
        )
    return (proc.stdout or "").strip()


def resolve_branch_node_id(
    known_ids: frozenset[str],
    *,
    cwd: Optional[str] = None,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
) -> str:
    """The one real graph node the current branch names - the bare verb's producer.

    A branch-derived candidate is a guess verified against the graph, the same
    rule ``bind_created_pr`` applies at bind time: exactly one well-formed
    segment must name a node ``known_ids`` carries. Zero (a non-node branch)
    and more than one (ambiguous) both refuse: the caller is about to MINT a
    closure claim, and one wrong id voids the whole binding at merge. Unlike
    ``known_node_ids`` the failure here is loud - the caller reads the
    exception, never an empty set - because silent empty is how a trailer-less
    PR ships and reds CI an hour later.
    """
    head_ref = _read_current_branch(cwd=cwd, runner=runner)
    if not head_ref:
        raise BranchResolutionError("current branch is unknown (detached HEAD?)")
    real = [nid for nid in branch_node_ids(head_ref) if nid in known_ids]
    if len(real) != 1:
        named = f" ({', '.join(real)})" if real else ""
        raise BranchResolutionError(
            f"branch '{head_ref}' names {len(real)} real node(s){named}; "
            "bare resolution needs exactly one - pass the node explicitly instead"
        )
    return real[0]


def ensure_closure_trailer(
    body: str,
    head_ref: str,
    *,
    extra_ids: Optional[list[str]] = None,
    known_ids: Optional[Iterable[str]] = None,
) -> str:
    """``body`` with an exact trailer claiming every node id in ``head_ref``.

    The one call a `gh pr create` path makes so the CI gate never reds a PR over
    a line the generator could have written. Returns the body unchanged when the
    ref names no node or the last trailer already claims them all, so a caller
    applies it unconditionally and a re-run changes nothing. A graph read that
    fails RAISES (through ``known_node_ids``): a dead reader stops the PR, it
    never opens one untrailered.

    Appends rather than rewrites: ``parse_closure_trailer`` and the gate both
    read the LAST trailer line, so a new final line wins without touching what
    an author already wrote.

    A branch-derived candidate is a GUESS and is verified against the graph
    before it is claimed; ``extra_ids`` is a caller's ASSERTION that those nodes
    ship here, so it is trusted. That asymmetry is the whole point: the CI gate
    may be liberal because it only DEMANDS a claim, but a producer that MINTS
    one has to be right. ``branch_node_ids("feature/x-dddd-cache-dead")`` yields
    ``cache-dead`` from ordinary English, and claiming it made every real claim
    on the line void at merge while CI stayed green.

    ``known_ids`` defaults to reading the graph, so a caller cannot skip the
    check by forgetting an argument. Pass an explicit set to stay pure.
    Nothing to verify means nothing to read: with no branch candidate the graph
    read is skipped, because the batch path passes no head ref at all and paid
    two git subprocesses and 2127 ids to filter an empty list.
    ``contained_in`` descendants remain ``render_pr_closure_trailer``'s job.
    """
    text = body if isinstance(body, str) else ""
    candidates = branch_node_ids(head_ref)
    if known_ids is not None:
        known = frozenset(known_ids)
    else:
        known = known_node_ids() if candidates else frozenset()
    wanted = list(
        dict.fromkeys(
            [n for n in candidates if n in known]
            + [e for e in (extra_ids or []) if is_wellformed_node_id(e)]
        )
    )
    if not wanted:
        return text
    claimed = parse_closure_trailer(text)
    if all(node_id in claimed for node_id in wanted):
        return text
    line = render_closure_trailer(claimed + wanted)
    if not line:
        return text
    return f"{text.rstrip()}\n\n{line}\n" if text.strip() else f"{line}\n"


# ---------------------------------------------------------------------------
# Query: the PR body + merge context, one gh call.
# ---------------------------------------------------------------------------

GH_QUERY_TIMEOUT_S = 30.0


@dataclass
class PrClosureContext:
    number: int
    body: str
    url: Optional[str]
    state: str
    merged_at: Optional[str]
    changed_files: list[str] = field(default_factory=list)


class ClosureQueryError(Exception):
    """Raised on gh failure while fetching a PR's closure context."""


def fetch_pr_closure_context(
    pr_number: int,
    *,
    repo: Optional[str] = None,
    cwd: Optional[str] = None,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
    timeout_s: float = GH_QUERY_TIMEOUT_S,
) -> PrClosureContext:
    """Shell out to ``gh pr view`` ONCE for body + merge state (AC3-HP: "the
    PR is queried once"). Raises :class:`ClosureQueryError` on any gh failure.
    """
    import json
    import shutil

    if shutil.which("gh") is None:
        raise ClosureQueryError("gh CLI not found on PATH")
    cmd = ["gh", "pr", "view", str(pr_number)]
    if repo:
        cmd += ["--repo", repo]
    cmd += ["--json", "number,body,url,state,mergedAt,files"]
    try:
        result = runner(
            cmd, capture_output=True, text=True, check=False, timeout=timeout_s, cwd=cwd
        )
    except subprocess.TimeoutExpired as exc:
        raise ClosureQueryError(
            f"gh pr view #{pr_number} timed out after {timeout_s}s"
        ) from exc
    except OSError as exc:
        raise ClosureQueryError(f"gh subprocess failed to launch: {exc}") from exc
    if result.returncode != 0:
        raise ClosureQueryError(
            f"gh pr view #{pr_number} failed (rc={result.returncode}): "
            f"{(result.stderr or '').strip()}"
        )
    stdout = result.stdout or ""
    if not stdout.strip():
        # An exit-0 gh call with blank stdout (truncated pipe, a shim that
        # swallowed the verb) is indistinguishable from a real empty answer -
        # never read a bare exit 0 as permission (AGENTS.md pitfalls corpus).
        # The old deleted `_pr_url` helper checked this explicitly; folding
        # it into "{}" here silently reversed the fail-closed guarantee this
        # module's callers depend on into fail-open.
        raise ClosureQueryError(f"gh pr view #{pr_number} returned no output (exit 0)")
    try:
        row = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise ClosureQueryError(f"gh stdout was not JSON: {exc}") from exc
    raw_files = row.get("files") or []
    changed_files = [
        item.get("path") if isinstance(item, dict) else item
        for item in raw_files
        if isinstance(item, str) or isinstance(item, dict)
    ]
    return PrClosureContext(
        number=row.get("number", pr_number),
        body=row.get("body") or "",
        url=row.get("url"),
        state=row.get("state", "UNKNOWN"),
        merged_at=row.get("mergedAt"),
        changed_files=[path for path in changed_files if isinstance(path, str) and path],
    )


# ---------------------------------------------------------------------------
# Bind: attach a validated claim set to every claimed node, all-or-nothing.
# ---------------------------------------------------------------------------


@dataclass
class ClosureBinding:
    node_id: str
    action: str  # "filled_primary" | "appended_additional" | "already_bound" | "already_done"


@dataclass
class ClosureBindResult:
    outcome: str  # "bound" | "refused"
    claimed_ids: list[str] = field(default_factory=list)
    bindings: list[ClosureBinding] = field(default_factory=list)
    refusal: Optional[str] = None

    @property
    def bound_ids(self) -> list[str]:
        return [
            b.node_id
            for b in self.bindings
            if b.action in ("filled_primary", "appended_additional")
        ]


def bind_closure_claims(
    entries: list[dict],
    claimed_ids: list[str],
    *,
    pr_number: int,
    pr_url: Optional[str],
    repo: Optional[str] = None,
    owner: Optional[str] = None,
) -> ClosureBindResult:
    """Validate every claimed id, then bind all of them - or mutate nothing.

    AC3-ERR: an unknown, malformed, or cross-repo claim refuses the WHOLE
    binding before any node mutates - a partial bind would leave the graph
    in a state no single review ever approved. AC3-EDGE: a node that already
    carries a different primary PR appends to ``additional_prs`` rather than
    clobbering it. A node already carrying THIS exact PR ref, or already
    done, is a no-op for that id (AC4-EDGE: a second reconcile of the same
    trailer reports zero new bindings, not an error).

    Cross-repo is judged from each claimed node's OWN existing PR refs only
    (never extra I/O per claim): a node with no PR ref yet has no known repo
    and is accepted, mirroring ``_find_pr_node_id``'s best-effort stance.
    """
    from fno.graph._reconcile import bind_pr_rows

    result = bind_pr_rows(
        entries,
        claimed_ids,
        pr_number=pr_number,
        pr_url=pr_url,
        repo=repo,
        owner=owner,
    )
    return ClosureBindResult(
        outcome=result.outcome,
        claimed_ids=result.claimed_ids,
        bindings=[ClosureBinding(b.node_id, b.action) for b in result.bindings],
        refusal=result.refusal,
    )


def bind_created_pr(
    entries: list[dict],
    *,
    head_ref: str,
    pr_url: str,
    owner: Optional[str] = None,
    node_id: Optional[str] = None,
) -> ClosureBindResult:
    """Bind one newly-created PR to its one real node.

    ``node_id`` is the caller's authoritative identity - the target manifest's
    graph_node_id. It LEADS, and the branch is only a fallback, because a branch
    is free text: one that never carried the id resolves to nothing, and a reused
    or handed-off worktree still carries the PREVIOUS node's id and would bind
    the PR to it.

    With no ``node_id``, branch text is a candidate only. Exactly one well-formed
    segment must name a node in this graph; zero or several refuse before any
    node changes. The URL must carry both a repository and PR number. Repeating
    the same observation is idempotent.
    """
    from fno.graph._reconcile import (
        pr_number_from_url,
        repo_slug_from_url,
    )

    pr_number = pr_number_from_url(pr_url)
    repo = repo_slug_from_url(pr_url)
    if pr_number is None or repo is None:
        return ClosureBindResult(
            outcome="refused", refusal="created PR URL is malformed or unscoped"
        )

    real_ids = {
        entry.get("id")
        for entry in entries
        if isinstance(entry, dict) and isinstance(entry.get("id"), str)
    }
    if isinstance(node_id, str) and node_id.strip() in real_ids:
        matched = [node_id.strip()]
    else:
        matched = list(dict.fromkeys(nid for nid in branch_node_ids(head_ref) if nid in real_ids))
    if len(matched) != 1:
        return ClosureBindResult(
            outcome="refused",
            claimed_ids=matched,
            refusal=(
                "branch does not resolve to exactly one real node"
                if not matched
                else f"branch ambiguously names {len(matched)} real nodes"
            ),
        )

    result = bind_closure_claims(
        entries,
        matched,
        pr_number=pr_number,
        pr_url=pr_url,
        repo=repo,
        owner=owner,
    )
    if result.outcome != "bound":
        return result

    return result


def bind_created_pr_from_branch(
    pr_url: str,
    *,
    owner: Optional[str] = None,
    cwd: Optional[str] = None,
    head_ref: Optional[str] = None,
    node_id: Optional[str] = None,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
) -> ClosureBindResult:
    """Persist a created-PR binding atomically, manifest id first.

    A caller holding an authoritative ``node_id`` skips branch resolution
    entirely; the branch lookup below is the fallback for callers that do not.
    """
    from fno.graph.store import commit_rows_via_store, read_graph_strict
    from fno.paths import graph_json
    from fno.tracker import active_backend_name

    if active_backend_name() != "graph":
        return ClosureBindResult(outcome="refused", refusal="graph backend is not active")
    authoritative = node_id.strip() if isinstance(node_id, str) and node_id.strip() else None
    if head_ref is None and authoritative is None:
        try:
            head_ref = _read_current_branch(cwd=cwd, runner=runner)
        except BranchResolutionError as exc:
            return ClosureBindResult(outcome="refused", refusal=str(exc))
    head_ref = head_ref or ""
    if not head_ref and authoritative is None:
        return ClosureBindResult(outcome="refused", refusal="current branch is unknown")

    path = graph_json()
    try:
        snapshot = read_graph_strict(path)
    except Exception as exc:
        return ClosureBindResult(outcome="refused", refusal=f"graph read failed: {exc}")
    probe = bind_created_pr(
        copy.deepcopy(snapshot), head_ref=head_ref, pr_url=pr_url, owner=owner,
        node_id=authoritative,
    )
    if probe.outcome != "bound":
        return probe

    box: list[ClosureBindResult] = []

    def _mutate(entries: list[dict]) -> list[dict]:
        box.append(bind_created_pr(
            entries, head_ref=head_ref, pr_url=pr_url, owner=owner, node_id=authoritative,
        ))
        return entries

    commit_rows_via_store(path, _mutate)
    return box[0]
