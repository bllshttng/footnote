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
from dataclasses import dataclass, field
from typing import Callable, Optional

from fno.graph._constants import is_wellformed_node_id

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
    try:
        row = json.loads(result.stdout or "{}")
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
