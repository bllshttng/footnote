"""Runtime-attempt projection for ``fno backlog provenance`` (x-2ccd wave 3).

A READ-TIME join that answers "which session worked on this node?" by projecting
active or interrupted target attempts from manifests + claims, alongside the
confirmed lifecycle rows the graph already owns. It never writes a graph row and
never labels an inferred attempt as confirmed ``do`` (AC8-LOCK): runtime
attempts and confirmed lifecycle phases answer different questions.

Each attempt is classified ``live`` / ``suspect`` / ``stale`` / ``interrupted``:

- live        manifest identity, claim holder, and a live process anchor agree;
- suspect     a protected or unverifiable owner exists without affirmative
              evidence to call the attempt live/stale/interrupted;
- stale       the owner process and lease are dead with no meaningful work;
- interrupted the manifest-bound attempt has affirmative work (commits / a PR /
              implementation events), its owner is dead, and no successful
              delivery terminal exists.

A claim-rebound PID transition updates ONE row (dedup on ``fno_id``), never a
second attempt. Every provenance source that cannot be read is reported as
unavailable on its row rather than aborting the whole view.
"""
from __future__ import annotations

from pathlib import Path
from typing import Any, Callable, Optional

from fno.target.manifest import manifest_identity, read_target_manifest


def _default_worktree_roots(repo_root: Path) -> list[Path]:
    """Git-registered worktrees for ``repo_root`` plus the canonical checkout.

    Bounds the scan to declared worktrees (never an arbitrary home-directory
    walk). A git failure degrades to just the canonical root.
    """
    roots = [repo_root]
    try:
        from fno.runtime.worktree import list_worktrees

        for wt in list_worktrees(repo_root=repo_root):
            p = wt.get("worktree_path")
            if p:
                roots.append(Path(p))
    except Exception:  # noqa: BLE001 - git unavailable -> canonical only
        pass
    # Dedup, preserve order.
    seen: set[str] = set()
    out: list[Path] = []
    for r in roots:
        key = str(r)
        if key not in seen:
            seen.add(key)
            out.append(r)
    return out


def _commits_ahead(repo_root: Path, branch: Optional[str]) -> Optional[int]:
    """Commits on ``branch`` ahead of origin/main (origin/master when main is
    absent), or None when unknowable.

    Affirmative authored-work evidence for the ``interrupted`` label. Best-effort:
    a detached/missing branch or a git failure yields None, not a zero that would
    suppress the interrupted verdict.
    """
    if not branch:
        return None
    try:
        import subprocess

        for base_ref in ("origin/main", "origin/master"):
            res = subprocess.run(
                ["git", "rev-list", "--count", f"{base_ref}..{branch}"],
                cwd=str(repo_root),
                capture_output=True,
                text=True,
                timeout=5,
            )
            if res.returncode != 0:
                continue
            n = res.stdout.strip()
            if n.isdigit():
                return int(n)
        return None
    except Exception:  # noqa: BLE001
        return None


def _classify(
    claim_state: Optional[str],
    has_work: bool,
    node_terminal: bool,
) -> str:
    """The conservative attempt classifier (separate from raw claim_state)."""
    if claim_state == "live":
        return "live"
    if claim_state in ("suspect", "stale", "free", "corrupted", None):
        # Affirmative work whose owner is gone and that never reached a delivery
        # terminal is an interrupted attempt, regardless of the raw claim state
        # (the claim may have lapsed to free while the work persists on the branch).
        if has_work and not node_terminal:
            return "interrupted"
        if claim_state == "suspect":
            return "suspect"
        return "stale"
    return "suspect"


def runtime_attempts(
    node_id: str,
    node: dict[str, Any],
    *,
    repo_root: Optional[Path] = None,
    worktree_roots: Optional[list[Path]] = None,
    claim_state_fn: Optional[Callable[[str], dict[str, Any]]] = None,
) -> list[dict[str, Any]]:
    """Project runtime attempts for ``node_id`` from manifests + the claim.

    ``node`` is the graph node dict (for cwd, pr_number, status). Returns one row
    per manifest whose ``graph_node_id`` matches, deduplicated by ``fno_id``. The
    claim is read once (it is global for ``node:`` keys); inject ``claim_state_fn``
    for tests so the real claims store is untouched.
    """
    cwd = node.get("cwd")
    root = Path(repo_root or cwd or Path.cwd())
    roots = worktree_roots if worktree_roots is not None else _default_worktree_roots(root)

    claim_state_fn = claim_state_fn or _default_claim_state
    claim = claim_state_fn(node_id)
    claim_holder = claim.get("holder") if isinstance(claim, dict) else None
    live_claim_state = str(claim.get("state") or "").lower() or None
    node_terminal = str(node.get("status") or "").lower() in {"done", "superseded"}
    pr_number = node.get("pr_number")

    rows: dict[str, dict[str, Any]] = {}  # dedup on fno_id
    for wt in roots:
        raw = read_target_manifest(wt)
        ident = manifest_identity(raw)
        if ident is None or ident.get("graph_node_id") != node_id:
            continue
        fno_id = ident["fno_id"]
        commits = _commits_ahead(wt, _branch_from_manifest(raw))
        has_work = bool(commits) or bool(pr_number)
        # The node claim is global and singular, so it can name at most ONE
        # attempt's owner. Only the manifest whose target_claim_holder matches
        # the live claim holder may inherit the claim's state; every other
        # manifest on disk is a HISTORICAL attempt (a prior run, or a sibling
        # worktree whose session lost the node) and must classify from its own
        # work evidence, never from a claim a different session now holds.
        # Without this gate a newer live attempt would relabel an old
        # interrupted attempt as "live" (the mislabeling this module exists to
        # prevent).
        is_current_owner = bool(claim_holder) and claim_holder == ident.get(
            "target_claim_holder"
        )
        if is_current_owner:
            row_claim_state = live_claim_state
            row_claim_holder = claim_holder
            row_claim_pid = claim.get("pid")
        else:
            row_claim_state = None
            row_claim_holder = ident.get("target_claim_holder")
            row_claim_pid = None
        attempt_state = _classify(row_claim_state, has_work, node_terminal)
        # Dedup: keep the row with the most affirmative evidence / freshest state.
        prev = rows.get(fno_id)
        if prev is not None and _rank(prev["attempt_state"]) >= _rank(attempt_state):
            continue
        rows[fno_id] = {
            "fno_id": fno_id,
            "harness": ident["harness"],
            "harness_session_id": ident["harness_session_id"],
            "node": node_id,
            "worktree": str(wt),
            "claim_state": row_claim_state,
            "claim_holder": row_claim_holder,
            "claim_pid": row_claim_pid,
            "commits_ahead": commits,
            "pr_number": pr_number,
            "attempt_state": attempt_state,
            "lifecycle": "unconfirmed do",  # never a confirmed lifecycle row
        }
    return list(rows.values())


def _rank(state: str) -> int:
    """Higher = more affirmative; used to pick the surviving dedup row."""
    return {"live": 4, "interrupted": 3, "suspect": 2, "stale": 1}.get(state, 0)


def _branch_from_manifest(raw: Optional[dict[str, Any]]) -> Optional[str]:
    """Best-effort branch name from a manifest (the orienter/receipt may record it)."""
    if not raw:
        return None
    for key in ("worktree_branch", "branch"):
        val = raw.get(key)
        if isinstance(val, str) and val.strip() and val.strip() != "null":
            return val.strip()
    return None


def _default_claim_state(node_id: str) -> dict[str, Any]:
    """Read the live ``node:<id>`` claim state (global root). Never raises."""
    try:
        from fno.claims import claim_status

        return claim_status(f"node:{node_id}")
    except Exception:  # noqa: BLE001
        return {}
