"""Exact `Backlog-Closure:` trailer: parse, render, and bind PR-to-node closure.

A merged PR's body may name several backlog nodes, but only the ONE node
stamped into `.fno/target-state.md` at creation ever gets its `pr_number`
written to the graph - every other named node stays open forever, because
the forward scan in `_reconcile.scan_merge_drift` needs a PR ref to query and
the reverse branch-name map only carries the primary node's id (x-59a6).

Free-text mentions ("this also fixes x-1234", "blocked by x-5678") are
measurement-only (see `scripts/metrics/pr-node-closure-audit.py`) and must
NEVER become a closure claim - a dependency note or a follow-up filing reads
identically to a close claim to a prose scanner. The exact trailer is the
only runtime-recognized closure grammar, so a claim is either the literal
line or it does not exist.
"""
from __future__ import annotations

import re
import subprocess
import sys
from dataclasses import dataclass, field
from typing import Callable, Iterable, Optional

from fno.graph._constants import NODE_ID_BODY, is_wellformed_node_id

TRAILER_KEY = "Backlog-Closure"

# Anchored to the START of a line (MULTILINE): a sentence merely containing
# "the Backlog-Closure trailer is..." mid-paragraph must never parse as the
# trailer itself, matching git trailer convention.
_TRAILER_LINE_RE = re.compile(
    rf"^{re.escape(TRAILER_KEY)}:[ \t]*(.*)$", re.IGNORECASE | re.MULTILINE
)


def parse_closure_trailer(body: str) -> list[str]:
    """Well-formed node ids named on the LAST exact ``Backlog-Closure:`` line.

    Order-preserved, deduplicated. Only a line that starts exactly with the
    trailer key counts (AC2-EDGE) - prose in a Dependencies/Follow-ups/
    Collisions section never becomes a claim, however it phrases a mention.
    Multiple trailer lines (e.g. after a rebase carried a stale one forward):
    only the LAST wins, mirroring git trailer semantics. A malformed token on
    an otherwise-good line (typo, stray punctuation) is silently dropped here;
    the CI backstop (``check-pr-node-closure.sh``) is what enforces
    well-formedness at PR-open time, not this runtime parser refusing an
    otherwise-legitimate merge over one bad token.
    """
    if not isinstance(body, str) or not body:
        return []
    lines = _TRAILER_LINE_RE.findall(body)
    if not lines:
        return []
    ids: list[str] = []
    seen: set[str] = set()
    for token in lines[-1].replace(",", " ").split():
        if is_wellformed_node_id(token) and token not in seen:
            seen.add(token)
            ids.append(token)
    return ids


def render_closure_trailer(node_ids: list[str]) -> str:
    """The one place a trailer LINE is built, so parse<->render round-trips.

    Drops malformed/duplicate ids; returns "" (no line) when nothing well-formed
    remains, so a caller can safely append the result to a body unconditionally.
    """
    ids = [n for n in dict.fromkeys(node_ids) if is_wellformed_node_id(n)]
    return f"{TRAILER_KEY}: {' '.join(ids)}" if ids else ""


def contained_descendant_ids(entries: list[dict], node_id: str) -> list[str]:
    """Every node whose ``contained_in`` points at ``node_id``, in graph order.

    These are units that ship INSIDE the same delivery (x-e957's convention),
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
# scanning is what makes the two agree on a ref like "feature/x-cdef-1234":
# once "x-cdef" is consumed the scan resumes at "-1234", which is not
# letter-led, so the bogus "cdef-1234" candidate the gate's skip-both-segments
# step exists to prevent is never produced on this side either.
_BRANCH_NODE_ID_RE = re.compile(rf"(?:^|[/-])({NODE_ID_BODY})(?=$|[/-])")


def branch_node_ids(head_ref: str) -> list[str]:
    """Well-formed node ids named as delimiter-bounded segments of ``head_ref``.

    Order-preserved, deduplicated. A bare substring never counts - fixed-width
    hex makes ``x-5b66`` a prefix of ``x-5b667`` - which is the same rule
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
    """Every id the graph actually carries; empty when it cannot be read.

    Empty is the SAFE direction. With nothing verified, no branch-derived
    candidate is claimed and the CI gate reds loudly, which a human can see and
    act on. The alternative is a trailer naming an id the graph does not carry:
    that PASSES CI, and then ``bind_closure_claims`` refuses the WHOLE binding
    at merge, so the real node never closes and nothing says so.
    """
    try:
        from fno.graph.store import read_graph
        from fno.paths import graph_json
        from fno.tracker import active_backend_name

        if active_backend_name() != "graph":
            # graph.json is not the delivery record of truth under an external
            # tracker, which is the same posture `fno pr closure-trailer` takes
            # there. Nothing to verify against, so nothing is claimed.
            return frozenset()
        return frozenset(
            e["id"]
            for e in read_graph(graph_json())
            if isinstance(e, dict) and isinstance(e.get("id"), str)
        )
    except Exception as exc:
        # Say so. Returning empty silently turns the producer into a no-op:
        # no trailer is written, the PR opens, and the only symptom is a red
        # gate that names the branch rather than the read that failed.
        print(f"fno: closure trailer cannot read the graph ({exc}); "
              f"claiming no branch-derived node", file=sys.stderr)
        return frozenset()


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
    applies it unconditionally and a re-run changes nothing.

    Appends rather than rewrites: ``parse_closure_trailer`` and the gate both
    read the LAST trailer line, so a new final line wins without touching what
    an author already wrote.

    A branch-derived candidate is a GUESS and is verified against the graph
    before it is claimed; ``extra_ids`` is a caller's ASSERTION that those nodes
    ship here, so it is trusted. That asymmetry is the whole point: the CI gate
    may be liberal because it only DEMANDS a claim, but a producer that MINTS
    one has to be right. ``branch_node_ids("feature/x-49ec-cache-dead")`` yields
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
    cmd += ["--json", "number,body,url,state,mergedAt"]
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
    return PrClosureContext(
        number=row.get("number", pr_number),
        body=row.get("body") or "",
        url=row.get("url"),
        state=row.get("state", "UNKNOWN"),
        merged_at=row.get("mergedAt"),
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
    from fno.graph._intake import _find_node
    from fno.graph._reconcile import node_is_open, node_pr_refs, repo_slug_from_url

    if not claimed_ids:
        return ClosureBindResult(outcome="refused", refusal="no closure claims to bind")

    our_repo = repo or repo_slug_from_url(pr_url)

    nodes: dict[str, dict] = {}
    for nid in claimed_ids:
        if not is_wellformed_node_id(nid):
            return ClosureBindResult(
                outcome="refused",
                claimed_ids=claimed_ids,
                refusal=f"malformed claim: {nid!r}",
            )
        node = _find_node(entries, nid)
        if node is None:
            return ClosureBindResult(
                outcome="refused",
                claimed_ids=claimed_ids,
                refusal=f"unknown node: {nid}",
            )
        existing_refs = node_pr_refs(node)
        if our_repo:
            for _num, url in existing_refs:
                existing_repo = repo_slug_from_url(url)
                if existing_repo is None:
                    # A stale ref can carry pr_number with no parseable
                    # pr_url (e.g. a pre-repo-scoping stamp). our_repo IS
                    # known here, so silently treating this as "no conflict"
                    # is the same silent-wrong-close the definite-mismatch
                    # branch below refuses - fail closed instead.
                    return ClosureBindResult(
                        outcome="refused",
                        claimed_ids=claimed_ids,
                        refusal=(
                            f"{nid} already carries a PR #{_num} ref with no "
                            "resolvable repo; this PR is "
                            f"{our_repo} - refusing an unverifiable cross-repo claim"
                        ),
                    )
                if existing_repo.lower() != our_repo.lower():
                    return ClosureBindResult(
                        outcome="refused",
                        claimed_ids=claimed_ids,
                        refusal=(
                            f"{nid} already carries a {existing_repo} PR ref; "
                            f"this PR is {our_repo} - refusing a cross-repo claim"
                        ),
                    )
        elif existing_refs:
            # our_repo is unresolvable (no --repo, no parseable pr_url), so the
            # cross-repo check above cannot run at all. A node with NO existing
            # ref is still safe to accept (nothing to collide with); a node
            # that already carries a ref is not - binding blind here is the
            # exact silent-wrong-close this function otherwise refuses. Fail
            # closed instead of skipping the check.
            return ClosureBindResult(
                outcome="refused",
                claimed_ids=claimed_ids,
                refusal=(
                    f"{nid} already carries a PR ref and this PR's repo is "
                    "unresolvable - refusing an unscoped claim"
                ),
            )
        nodes[nid] = node

    bindings: list[ClosureBinding] = []
    for nid in claimed_ids:
        node = nodes[nid]
        refs = node_pr_refs(node)
        if any(num == pr_number for num, _ in refs):
            bindings.append(ClosureBinding(nid, "already_bound"))
            continue
        if not node_is_open(node):
            bindings.append(ClosureBinding(nid, "already_done"))
            continue
        if not isinstance(node.get("pr_number"), int):
            node["pr_number"] = pr_number
            node["pr_url"] = pr_url
            bindings.append(ClosureBinding(nid, "filled_primary"))
        else:
            existing_list = list(node.get("additional_prs") or [])
            existing_list.append({"number": pr_number, "url": pr_url})
            node["additional_prs"] = existing_list
            bindings.append(ClosureBinding(nid, "appended_additional"))

    return ClosureBindResult(outcome="bound", claimed_ids=claimed_ids, bindings=bindings)
