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
from dataclasses import dataclass
from datetime import datetime, timezone
from functools import lru_cache
from pathlib import Path
from typing import Any, Callable, Literal, Optional

# `gh pr view` default timeout; covers network hangs and stuck auth prompts.
GH_QUERY_TIMEOUT_S = 30.0


@lru_cache(maxsize=1)
def _gh_executable() -> Optional[str]:
    """Resolve the gh binary path once per process.

    query_pr_merge_state runs per-PR inside scan_merge_drift's loop, so a
    bare shutil.which("gh") would re-scan PATH on every node. Cached here.
    """
    return shutil.which("gh")

# Extract `owner/repo` from a GitHub PR/issue URL. A trailing `.git` is never
# present on web URLs but stripped defensively. Stops before `/pull/<n>`.
_PR_URL_RE = re.compile(r"github\.com/([^/\s]+)/([^/\s]+?)(?:\.git)?/(?:pull|issues)/")


def repo_slug_from_url(url: Optional[str]) -> Optional[str]:
    """Return ``owner/repo`` parsed from a GitHub PR URL, or None.

    Used to scope ``gh pr view`` to the node's actual repository via
    ``--repo``. Without this, gh resolves the PR number against whatever repo
    the process happens to run in, so a same-number PR in a different repo
    could be mistaken for the node's PR.
    """
    if not url:
        return None
    m = _PR_URL_RE.search(url)
    return f"{m.group(1)}/{m.group(2)}" if m else None


# `owner/repo` from a remote URL: git@github.com:owner/repo.git or
# https://github.com/owner/repo(.git).
_REMOTE_SLUG_RE = re.compile(r"(?:github\.com[:/])([^/]+)/(.+?)(?:\.git)?/?$")


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
        m = _REMOTE_SLUG_RE.search(out.strip())
        if m:
            return f"{m.group(1)}/{m.group(2)}"
    rc, out = runner(
        ["gh", "repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"], cwd
    )
    return (out.strip() or None) if rc == 0 else None

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

    @property
    def closeable(self) -> bool:
        return self.error is None and self.pr_state == "MERGED"


def node_is_open(node: dict) -> bool:
    """A node is open when it is neither done nor superseded.

    Keyed off the underlying fields rather than the derived ``_status`` so the
    predicate holds even on an entries list that has not been through
    ``recompute_statuses`` (e.g. a raw test fixture).
    """
    return not node.get("completed_at") and not node.get("superseded_by")


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
    cmd += ["--json", "number,state,url,mergedAt,mergeCommit"]
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

    return PrMergeState(
        number=row.get("number", pr_number),
        state=row.get("state", "UNKNOWN"),
        url=row.get("url"),
        merged_at=row.get("mergedAt"),
        merge_sha=(row.get("mergeCommit") or {}).get("oid"),
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


def reverse_map_unstamped(
    entries: list[dict],
    *,
    node_id: Optional[str] = None,
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
        if node_id is not None and nid != node_id:
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
    for cwd, nodes in by_cwd.items():
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
    node_id: Optional[str] = None,
    list_merged: Optional[Callable[..., list[dict]]] = None,
) -> list[MergeDriftRecord]:
    """Find open nodes whose PR has merged outside the ship gate.

    Returns one record per open node that resolves to a MERGED PR, plus
    records flagged with ``error`` for nodes whose PR state could not be
    resolved. Nodes whose PRs are all still OPEN (or closed-unmerged) yield no
    record - they are not drift. ``node_id`` restricts the scan to a single
    node. Tests inject a ``query`` stub to avoid shelling out to gh.

    A second pass (``reverse_map_unstamped``) covers open nodes with NO PR ref
    at all - a session that died before the node<->PR stamp - by matching the
    node id against merged branch names. ``list_merged`` is injected in tests.
    """
    if query is None:
        query = query_pr_merge_state

    records: list[MergeDriftRecord] = []

    for node in entries:
        nid = node.get("id")
        if not isinstance(nid, str):
            continue
        if node_id is not None and nid != node_id:
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
        wanted = [b for b in (required_bots or []) if isinstance(b, str) and b.strip()]
        if not wanted:
            return None  # AC2: nothing required, so nothing to escape

        # Resolve the events log NOW - before the review fetch - so a fetch
        # failure still knows where to write the durable emit-failure counter
        # (AC7). If this were resolved only after the fetch, a gh-auth blind spot
        # would log nothing and retro would read a silent zero (the exact
        # silent-low-reading the metric exists to prevent). Resolved AFTER the
        # no-required-bots return above, so a clean repo never touches the log.
        if resolved_events is None:
            resolved_events = _canonical_events_path(record.cwd)

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


# ---------------------------------------------------------------------------
# Post-merge-ritual auto-dispatch (x-47be, task 2.1 + 2.2)
# ---------------------------------------------------------------------------
#
# When ``config.post_merge.auto_run`` is armed, a merge detected by reconcile
# (and, fast-follow, pr_watch) dispatches ONE background ``/fno:pr merged <n>``
# worker for the merged PR - the same subscription-lane bg substrate ``/target
# bg`` uses, never ``-p``. That worker runs the full ritual, including the
# canonical-sync step. Exactly-once per merge SHA via an atomic-exclusive
# dispatch marker under the canonical ``.fno/post-merge-dispatched/<key>``, so
# overlapping reconciles / pr_watch spawn at most one worker (AC1-FR). Strictly
# non-fatal: a spawn failure drops the marker (a later reconcile retries) and
# never fails the caller.

_POST_MERGE_DISPATCH_SUBDIR = "post-merge-dispatched"


@dataclass
class PostMergeDispatchResult:
    outcome: Literal[
        "dispatched", "routed-warm", "finalized-origin",
        "already-dispatched", "disabled", "spawn-failed",
    ]
    pr_number: int
    short_id: Optional[str] = None
    detail: Optional[str] = None


def _dispatch_marker(canonical: Path, key: str) -> Path:
    return canonical / ".fno" / _POST_MERGE_DISPATCH_SUBDIR / key


# Single-flight dispatch-lock TTL: covers the spawn round-trip with margin, so a
# reconcile killed mid-spawn self-heals (the persistent marker is written only on
# success, and the lock recovers on TTL expiry) instead of wedging the ritual.
_POST_MERGE_DISPATCH_TTL_MS = 15 * 60 * 1000


def _origin_transcript_path(
    session_id: Optional[str], cwd: Optional[str], harness: Optional[str]
) -> Optional[Path]:
    """The on-disk claude transcript for a ``(session, cwd)`` origin, or None.

    Liveness-independent: pure filesystem existence, deliberately NOT
    ``discover_live_sessions`` (which keys on a live process — exactly what a
    dead origin lacks). Reuses discover's cwd->projects-subdir encoding + its
    projects-store constant rather than re-deriving the transcript path here
    (the encoding is non-obvious, the classic silent-miss). Claude-first: a
    non-claude harness yields None, so a codex/gemini origin falls through to
    cold + the backstop.
    """
    if not session_id or not cwd:
        return None
    if harness and harness != "claude":
        return None
    try:
        from fno.agents.discover import _candidate_dir_names, default_projects_dir

        projects = default_projects_dir()
        names = list(_candidate_dir_names(cwd))
    except Exception:  # noqa: BLE001 - discover unavailable/erroring -> no probe
        return None
    for name in names:
        candidate = projects / name / f"{session_id}.jsonl"
        if candidate.is_file():
            return candidate
    return None


def origin_transcript_exists(
    session_id: Optional[str], cwd: Optional[str], harness: Optional[str]
) -> bool:
    """True iff the origin's transcript AND its ``target-state.md`` both survive.

    The direct-finalize rung's gate (x-88df US4): finalize reads the manifest
    and the transcript entirely from disk, so both must exist for the rung to
    produce a full-fidelity row. A missing manifest (archived worktree) returns
    False and dispatch falls to cold + the reconcile backstop.
    """
    if _origin_transcript_path(session_id, cwd, harness) is None:
        return False
    return (Path(cwd) / ".fno" / "target-state.md").is_file()


def _finalize_origin_ledger(
    source_cwd: str, transcript: str, harness: Optional[str]
) -> bool:
    """Invoke ``fno-agents finalize`` against a dead origin's manifest+transcript.

    Writes the full-fidelity ledger row (cost/tokens/phases/provider) + repairs
    ``pr_number``, with no session revival. ``--reason DoneAwaitingMerge`` is
    load-bearing: it is NOT a SHIP_REASON, so finalize runs the always-branch
    ledger row only, never re-running plan-stamp/handoff/verify_advise against
    the dead origin. Returns True on exit 0; any failure returns False so
    dispatch degrades to the cold spawn.
    """
    try:
        from fno.agents.rust_runtime import resolve_binary

        binary = resolve_binary()
    except Exception:  # noqa: BLE001 - runtime resolver unavailable
        binary = None
    if binary is None:
        return False
    state = Path(source_cwd) / ".fno" / "target-state.md"
    cmd = [
        str(binary), "finalize",
        "--state", str(state),
        "--cwd", str(source_cwd),
        "--reason", "DoneAwaitingMerge",
        "--transcript", str(transcript),
    ]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=180)
    except (OSError, subprocess.SubprocessError):
        return False
    return proc.returncode == 0


def dispatch_post_merge_ritual(
    pr_number: int,
    *,
    dedup_key: Optional[str] = None,
    auto_run: bool = False,
    node_cwd: Optional[str] = None,
    canonical_root: Optional[Path] = None,
    spawn: Optional[Callable[[int, str], str]] = None,
    source_session_id: Optional[str] = None,
    source_harness: Optional[str] = None,
    source_cwd: Optional[str] = None,
    warm_inject: Optional[Callable[[str, int, Optional[str]], "tuple[bool, str]"]] = None,
    finalize_origin: Optional[Callable[[str, str, Optional[str]], bool]] = None,
    notify_origin: Optional[Callable[[str, int, Optional[str], str], None]] = None,
) -> PostMergeDispatchResult:
    """Dispatch a bg ``/fno:pr merged <n>`` worker at most once per merge.

    ``dedup_key`` is the merge SHA when known (reconcile threads it from
    ``mergeCommit.oid``); it falls back to ``pr-<n>`` for a reverse-mapped
    record that has no SHA. ``auto_run`` gates the whole thing (opt-in).

    ``node_cwd`` is the closed node's recorded root: the canonical of THAT repo
    is resolved from it, so a full-graph reconcile run from project A that closes
    a project-B node still dispatches into B (both the dedup marker and the
    worker's ``--cwd`` target B's canonical, never A's). ``canonical_root``
    overrides the resolution outright (tests). The ``spawn`` seam is injected in
    tests so no real ``fno agents spawn`` fires.

    Exactly-once at two layers, both under the target repo's canonical: a
    persistent ``.fno/post-merge-dispatched/<key>`` marker (cross-session /
    cross-trigger) written ONLY after a successful hand-off, guarded by a TTL
    single-flight claim (concurrent detections) that self-heals a crash.

    ``source_session_id`` (the merged node's originating session) arms the
    warm route: a live inject of the ritual into that session, tried under
    the same lock/marker, so at most one of {warm inject, cold spawn} runs
    per merge -- a queued-but-unconfirmed inject counts as warm too (the
    keystrokes already landed). ``source_harness`` (claude|codex|gemini)
    selects which live vehicle reaches it, so a codex/gemini-shipped node
    routes warm to its own panel instead of always cold-spawning a claude
    worker. ``warm_inject`` is a test seam.
    """
    if not auto_run:
        return PostMergeDispatchResult("disabled", pr_number)

    if canonical_root is None:
        from fno.paths import (
            resolve_canonical_repo_root,
            resolve_canonical_worktree,
        )

        if node_cwd:
            wt = resolve_canonical_worktree(cwd=Path(node_cwd))
            canonical_root = Path(wt) if wt is not None else Path(node_cwd)
        else:
            canonical_root = resolve_canonical_repo_root()
    canonical = Path(canonical_root)

    key = re.sub(r"[^A-Za-z0-9._-]", "_", dedup_key or f"pr-{pr_number}")
    marker = _dispatch_marker(canonical, key)

    # Cross-session / cross-trigger dedup: a persisted marker means the ritual
    # already ran for this merge SHA.
    if marker.exists():
        return PostMergeDispatchResult("already-dispatched", pr_number, detail="marker-exists")

    from fno import claims

    def _persist_marker() -> None:
        # Persist the cross-session marker ONLY after a successful hand-off, so
        # a crash before this point leaves no marker (the sync stays recoverable
        # via a direct primitive call / manual ritual).
        try:
            marker.parent.mkdir(parents=True, exist_ok=True)
            marker.touch(exist_ok=True)
        except OSError:
            pass  # best-effort; a missing marker only re-dispatches (idempotent ritual)

    # Re-entrancy guard (x-616b): a live holder of the ritual's own Step-0.5
    # mutex means `/fno:pr merged <n>` is already running for this PR somewhere -
    # often the very session whose Step-2 reconcile invoked us. Read it via
    # claims_root_for (the same GLOBAL root the skill's `fno claim acquire
    # reconcile:pr-<n>` resolves to); reusing root=canonical here would silently
    # miss the skill's claim and the bug would survive the fix.
    from fno.claims.io import claims_root_for

    ritual_key = f"reconcile:pr-{pr_number}"
    try:
        # claim_status never raises, but claims_root_for -> Path.home() can
        # (RuntimeError with no HOME and no $FNO_CLAIMS_ROOT); fail-open like the
        # rest of this strictly-non-fatal function -> a None state falls through.
        ritual_state = claims.claim_status(ritual_key, root=claims_root_for(ritual_key)).get("state")
    except Exception:  # noqa: BLE001 - the guard must never break dispatch
        ritual_state = None
    if ritual_state == "live":
        # Strictly stronger evidence of hand-off than a spawn: the runner is not
        # merely spawned, it is executing. Write the marker so later pr_watch
        # ticks short-circuit on marker-exists instead of cold-firing a no-op.
        _persist_marker()
        return PostMergeDispatchResult(
            "already-dispatched", pr_number, detail="ritual-claim-live"
        )
    if ritual_state == "suspect":
        # TTL unexpired but holder pid dead: a crashed attended ritual. Reuse
        # pr_watch's lock-contention retry branch (no marker) so the watermark
        # does NOT advance; once the TTL expires the state degrades to stale and
        # the next tick dispatches a recovery worker.
        return PostMergeDispatchResult(
            "already-dispatched", pr_number, detail="lock-contention"
        )
    # free | stale | corrupted -> proceed unchanged (fail-open; the worker's own
    # Step 0.5 acquire remains the mutation authority).

    lock_key = f"post-merge-ritual:{key}"
    holder = f"reconcile-dispatch:{pr_number}"
    try:
        # Scope the claim to the target canonical (like the marker) so a
        # concurrent detection in the same repo - via reconcile OR the pr_watch
        # fast-follow - contends on the SAME lock.
        claims.acquire_claim(
            lock_key, holder, ttl_ms=_POST_MERGE_DISPATCH_TTL_MS,
            reason="post-merge ritual dispatch", root=canonical,
        )
    except claims.ClaimHeldByOther:
        # In-flight, NOT done: another detector holds the lock right now but may
        # still fail before writing the marker. Tag it so a polling caller
        # (pr-watch) does NOT treat this as a completed hand-off and stop
        # retrying - unlike marker-exists, which is a genuine completed dedup.
        return PostMergeDispatchResult("already-dispatched", pr_number, detail="lock-contention")

    try:
        if marker.exists():  # re-check under the lock (double-checked)
            return PostMergeDispatchResult("already-dispatched", pr_number, detail="marker-exists")

        # Warm route first: the node's originating session still holds the
        # context a cold worker re-derives. A genuine miss (no id, dead
        # session, inject error) degrades to the cold path below; a queued-
        # but-unconfirmed inject is handled separately below (not a miss).
        cold_reason = "no-live-source-session"
        # Gate for the direct-finalize rung below: only a CLEAN dead (None)
        # resolution reaches it, so a live origin is never direct-finalized
        # (AC1-EDGE). A warm-route exception leaves this False -> straight to cold.
        origin_dead = False
        try:
            from fno.post_merge_route import inject_pr_merged, resolve_warm_session

            warm_sid = resolve_warm_session(source_session_id, source_harness)
            origin_dead = warm_sid is None
            if warm_sid is not None:
                _inject = warm_inject if warm_inject is not None else inject_pr_merged
                delivered, reason = _inject(warm_sid, pr_number, source_harness)
                if delivered:
                    _persist_marker()
                    return PostMergeDispatchResult(
                        "routed-warm", pr_number, short_id=warm_sid[:8], detail=reason
                    )
                if reason == "queue-timeout":
                    # op:'reply' already typed the keystrokes into the PTY;
                    # just unconfirmed (recipient busy). Persist the marker
                    # same as a cold spawn does -- trust the hand-off, same
                    # as spawn() -- else nothing ever resolves this merge's
                    # dedup and a later call cold-spawns a redundant worker.
                    _persist_marker()
                    return PostMergeDispatchResult(
                        "routed-warm", pr_number, short_id=warm_sid[:8], detail="queued"
                    )
                cold_reason = reason
        except Exception as exc:  # noqa: BLE001 - warm routing must never break dispatch
            cold_reason = f"warm-error: {exc}"[:120]

        # Direct-finalize middle rung (x-88df): the origin is dead but its
        # manifest + transcript survive on disk. Finalize them directly - the
        # SAME writer the stop hook would have run, reached deterministically
        # (no session revival, no dependency on the stop hook's foreign-session
        # guard). Writes the origin's OWN full-fidelity ledger row (its real
        # cost/phases/provider), higher fidelity than any fresh ceremony thread.
        #
        # This writes ONLY the ledger row - it does NOT run the post-merge
        # ritual (retro harvest / parking-lot / canonical sync). So it does NOT
        # short-circuit dispatch: after finalizing, control FALLS THROUGH to the
        # cold spawn, which runs `/fno:pr merged` for those ritual steps exactly
        # as a dead origin got before this rung existed. The cold worker is not
        # a /target session, so it writes no second ledger row - finalize's row
        # stands (deduped by fno_id). A non-zero finalize degrades to cold as a
        # warm miss does. (Corrects a review P1: an early return here silently
        # skipped the ritual for every dead-origin merge.)
        finalized_origin = False
        if origin_dead and origin_transcript_exists(source_session_id, source_cwd, source_harness):
            _tpath = _origin_transcript_path(source_session_id, source_cwd, source_harness)
            if _tpath is not None:
                _finalize = finalize_origin if finalize_origin is not None else _finalize_origin_ledger
                try:
                    finalized_origin = _finalize(source_cwd, str(_tpath), source_harness)
                except Exception as exc:  # noqa: BLE001 - degrade to cold, never break dispatch
                    finalized_origin = False
                    cold_reason = f"finalize-error: {exc}"[:120]
                if cold_reason == "no-live-source-session":
                    cold_reason = (
                        "ledger direct-finalized; ritual cold"
                        if finalized_origin else "finalize-nonzero"
                    )

        if spawn is None:
            spawn = _spawn_post_merge_worker
        try:
            short_id = spawn(pr_number, str(canonical))
        except Exception as exc:  # noqa: BLE001 - non-fatal
            # No marker is written, so the sync stays recoverable via a later
            # DIRECT `fno pr sync-canonical --pr <n>` (its own SHA marker is
            # unwritten too) or a manual `/fno:pr merged <n>`. It is NOT
            # auto-retried by a later reconcile: reconcile only scans OPEN nodes
            # and this node is already closed, so a failed dispatch is a
            # once-only best-effort. The caller surfaces the recovery command;
            # robust auto-retry belongs to the pr_watch fast-follow (x-47be Q3).
            return PostMergeDispatchResult("spawn-failed", pr_number, detail=str(exc)[:200])
        _persist_marker()
        # Advisory: tell the origin session its PR merged. Cold path only (the
        # warm route already reached a live origin by running the ritual there),
        # inside the marker-guarded section so it fires at most once per merge
        # SHA. Best-effort: a mail failure must never break the dispatch or
        # withhold the marker -- the ritual hand-off is the load-bearing act.
        if source_session_id:
            _notify = notify_origin if notify_origin is not None else _notify_origin_merged
            try:
                _notify(source_session_id, pr_number, source_harness, cold_reason)
            except Exception as exc:  # noqa: BLE001 - advisory, never fatal
                _emit_origin_notify_failed(pr_number, source_session_id, str(exc)[:200], node_cwd)
        # "finalized-origin" when the ledger row came from the direct finalize
        # above (the ritual still ran cold); plain "dispatched" otherwise.
        return PostMergeDispatchResult(
            "finalized-origin" if finalized_origin else "dispatched",
            pr_number, short_id=short_id, detail=f"cold: {cold_reason}",
        )
    finally:
        try:
            claims.release_claim(lock_key, holder, root=canonical)
        except Exception:
            pass  # lock is TTL-bounded; a failed release recovers on its own


def _notify_origin_merged(
    session_id: str,
    pr_number: int,
    source_harness: Optional[str],
    cold_reason: str,
) -> None:
    """Send one durable ``fno mail`` to the origin session of a cold-dispatched
    merge. The cold path means the warm route already MISSED (the origin is not
    live-reachable), so the durable bus is the correct delivery floor -- a live
    origin would have been reached by the warm inject and this send is skipped.

    Function-local imports follow the ``_spawn_post_merge_worker`` pattern: a
    module-level ``fno.mail`` / ``fno.config`` import here would re-enter the
    config<->graph cycle and freeze ``read_graph``'s GRAPH_JSON fallback.
    """
    from fno.harness_identity import canonical_handle
    from fno.mail.cli import _name_lane_send

    harness = source_harness or "claude"
    recipient = canonical_handle(harness, session_id)
    msg = (
        f"PR #{pr_number} merged; the post-merge ritual was cold-dispatched "
        f"(you were not live: {cold_reason}). A bg worker ran /fno:pr merged; "
        f"continue your loop."
    )
    _name_lane_send(msg, from_name="fno", resolved=None, recipient=recipient, provider=harness)


def _emit_origin_notify_failed(
    pr_number: int, session_id: str, error: str, node_cwd: Optional[str]
) -> None:
    """A failed origin-notify is never silent (AC1-UI). The mail is advisory and
    the marker is already written, so a stderr diagnostic (the module's own
    best-effort idiom, cf. the human_touch emit) is the right surface -- a new
    registered event kind would be schema-parity overhead for an advisory miss.
    """
    print(
        f"pr-watch: origin-notify failed for PR #{pr_number} "
        f"(session {session_id[:8]}): {error}; merge dispatch unaffected",
        file=sys.stderr,
    )


def _spawn_post_merge_worker(pr_number: int, cwd: str) -> str:
    """Launch a detached ``claude --bg`` ``/fno:pr merged <n>`` worker.

    Mirrors advance's ``_spawn_worker``: the ``--substrate bg`` key is
    load-bearing (the post-x-3ab8 default ``pane`` substrate would stall a
    fire-and-forget dispatch at a placement prompt), and ``bg`` is claude-only.
    Returns the spawn receipt's short_id; raises on a non-zero spawn.
    """
    from fno import _subprocess_util

    # Function-local import: the config<->graph cycle means a top-level
    # `from fno.config import ...` freezes read_graph's GRAPH_JSON to the ~/.fno
    # fallback (same reason graph/cli.py imports it inside a try). Fail open to
    # the sonnet default - dispatch is strictly non-fatal.
    model = "claude-sonnet-5"
    try:
        from fno.config import load_settings_for_repo

        model = load_settings_for_repo(Path(cwd)).post_merge.model
    except Exception:
        pass

    name = f"pr-merged-{pr_number}"
    # `autonomous` is load-bearing: a `--bg` worker is INTERACTIVE, so without a
    # no-operator signal the ritual stalls at its first human-prompt slot
    # (x-47be v1's live stall). The signal rides the prompt - the one channel
    # that always reaches the detached worker's LLM (env need not propagate).
    cmd = [
        *_subprocess_util.fno_py_cmd(), "agents", "spawn",
        "--provider", "claude", "--substrate", "bg", "--model", model, "--cwd", cwd,
        name, f"/fno:pr merged {pr_number} autonomous",
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
    if proc.returncode != 0:
        raise RuntimeError(
            f"fno agents spawn exited {proc.returncode}: "
            f"{(proc.stderr or proc.stdout or '').strip()[:200]}"
        )
    for line in (proc.stdout or "").splitlines():
        if '"short_id"' in line:
            try:
                sid = str(json.loads(line).get("short_id", "") or "")
            except json.JSONDecodeError:
                continue
            if sid:
                return sid
    return "unknown"
