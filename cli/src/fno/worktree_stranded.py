"""The three-way stranded-worktree classifier (x-f4e9).

A provider-killed worker can leave finished commits with no branch, no PR
and no roster row. No single probe tells that apart from an abandoned
experiment or a live worker mid-run: git alone cannot (a rebase makes
shipped work read as unreachable), the graph alone cannot (an open node
says nothing about who, if anyone, is working it), and the fleet alone
cannot (it has no opinion on commits). Only the join of all three closes
the question, and closes it in the fail-open direction: a row where any
leg could not be read positively is UNKNOWN, reported, never acted on.

``classify()`` is a pure function over already-resolved inputs so it is
testable with a fixture table with no filesystem or subprocess involved.
``sweep()`` is the IO-doing driver a CLI verb or tick leg calls.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from fno.graph.fuzzy import resolve_node
from fno.graph.store import (
    GraphMalformedRootError,
    GraphUnreadableError,
    read_graph_strict,
)
from fno.paths import resolve_repo_root

# Same non-terminal AgentStatus vocabulary worktree-status.py uses (mirrored
# from crates/fno-agents/src/lib.rs `AgentStatus` /
# cli/src/fno/agents/registry.py `_OWNERSHIP_LIVE_STATUSES`). Duplicated here
# rather than imported because the two modules answer different questions
# from the same registry read (worktree-status.py: which session owns this
# path; here: is any input read trustworthy enough to act on), and the
# import boundary between a `scripts/lib/` script and this package is not
# worth crossing for one frozenset.
_ALIVE_STATUSES = frozenset({"spawning", "ready", "idle", "busy", "live", "restarting"})

_QUIET_TERMINAL_STATUSES = frozenset({"superseded", "deferred"})

# klass values, in the order classify() checks them. CLEAN and the five
# "quiet" classes are informational; only STRANDED is ever acted on, and
# UNKNOWN is the fail-open bucket that is reported but never acted on either.
CLEAN = "CLEAN"
UNKNOWN = "UNKNOWN"
SHIPPED = "SHIPPED"
ABANDONED = "ABANDONED"
LIVE = "LIVE"
PR_OPEN = "PR_OPEN"
STRANDED = "STRANDED"


@dataclass(frozen=True)
class Row:
    klass: str
    node: Optional[str]
    unpushed: int
    age: str
    facts: dict = field(default_factory=dict)


def classify(
    *,
    path: str,
    branch: Optional[str],
    unpushed: int,
    unpushed_ok: bool,
    node: Optional[str],
    node_entry: Optional[dict],
    graph_ok: bool,
    registry_status: Optional[str],
    registry_ok: bool,
    age: str = "unknown",
) -> Row:
    """First match wins. See module docstring for the shape of the join."""
    facts = {"path": path, "branch": branch}

    if unpushed == 0:
        return Row(CLEAN, node, unpushed, age, facts)

    if node is None or node_entry is None:
        return Row(UNKNOWN, node, unpushed, age, {**facts, "reason": "node unresolved"})

    if not (unpushed_ok and graph_ok and registry_ok):
        failed = [
            name
            for name, ok in (("git", unpushed_ok), ("graph", graph_ok), ("fleet", registry_ok))
            if not ok
        ]
        return Row(UNKNOWN, node, unpushed, age, {**facts, "reason": f"read failed: {','.join(failed)}"})

    if node_entry.get("status") == "done":
        return Row(SHIPPED, node, unpushed, age, facts)

    if node_entry.get("status") in _QUIET_TERMINAL_STATUSES:
        return Row(ABANDONED, node, unpushed, age, facts)

    if registry_status in _ALIVE_STATUSES:
        return Row(LIVE, node, unpushed, age, facts)

    if node_entry.get("pr_number"):
        return Row(PR_OPEN, node, unpushed, age, facts)

    return Row(STRANDED, node, unpushed, age, facts)


# --- node resolution ---------------------------------------------------


def _basename_candidate(path: str) -> str:
    return Path(path).name


def _branch_candidate(branch: Optional[str]) -> Optional[str]:
    """Last ``/``-delimited segment: `feature/x-fd2a` -> `x-fd2a`."""
    if not branch:
        return None
    return branch.rsplit("/", 1)[-1]


def _read_state_graph_node_id(path: str) -> Optional[str]:
    """The ``graph_node_id: <id>`` line appended to the manifest BODY by
    ``hooks/helpers/init-target-state.sh`` - outside the YAML frontmatter,
    so ``fno state show --field`` cannot return it."""
    state_file = Path(path) / ".fno" / "target-state.md"
    try:
        text = state_file.read_text(encoding="utf-8")
    except OSError:
        return None
    m = re.search(r"^graph_node_id:\s*(\S+)\s*$", text, re.MULTILINE)
    if not m:
        return None
    value = m.group(1)
    return None if value == "null" else value


def resolve_node_id(
    path: str, branch: Optional[str], entries_by_id: dict
) -> tuple[Optional[str], Optional[dict]]:
    """Worktree directory basename, then branch name, then the state-file's
    own recorded ``graph_node_id`` - first hit wins. Returns (id, entry);
    both None when nothing resolves or the resolved id has no graph row."""
    entries = list(entries_by_id.values())
    for candidate in (_basename_candidate(path), _branch_candidate(branch)):
        if not candidate:
            continue
        match = resolve_node(candidate, entries)
        if match.kind == "exact" and match.id:
            return match.id, entries_by_id.get(match.id)

    state_id = _read_state_graph_node_id(path)
    if state_id and state_id in entries_by_id:
        return state_id, entries_by_id[state_id]

    return None, None


# --- git input: one verified fetch per process, then per-path rev-list -


def _unpushed_batch(paths: list[str]) -> dict[str, tuple[int, bool]]:
    """path -> (unpushed_count, ok). Shells to the shared
    ``wt_unpushed_count`` (scripts/lib/worktree-unpushed.sh) rather than a
    second implementation of its fail-toward-keep contract. All paths run in
    one bash process so the script's own per-process fetch cache (exported
    ``_WT_REMOTE_REFS_FRESH``/``_STALE``) verifies the remote exactly once
    for the whole batch, not once per worktree."""
    if not paths:
        return {}
    repo_root = Path(resolve_repo_root())
    script = repo_root / "scripts" / "lib" / "worktree-unpushed.sh"
    driver = (
        'source "$1"; shift\n'
        'for p in "$@"; do\n'
        '  err="$(mktemp)"\n'
        '  out="$(wt_unpushed_count "$p" 2>"$err")"\n'
        '  errtext="$(cat "$err")"; rm -f "$err"\n'
        '  ok=1\n'
        '  case "$errtext" in *"not verifiable"*) ok=0 ;; esac\n'
        "  printf '%s\\x1f%s\\x1f%s\\n' \"$p\" \"$out\" \"$ok\"\n"
        "done\n"
    )
    proc = subprocess.run(
        ["bash", "-c", driver, "bash", str(script), *paths],
        capture_output=True,
        text=True,
    )
    results: dict[str, tuple[int, bool]] = {}
    for line in (proc.stdout or "").splitlines():
        parts = line.split("\x1f")
        if len(parts) != 3:
            continue
        p, count_s, ok_s = parts
        count = int(count_s) if count_s.isdigit() else 1
        results[p] = (count, ok_s == "1")
    # A path the driver never reported (bash itself failed) fails toward
    # "unpushed and unverifiable", the same posture wt_unpushed_count takes.
    for p in paths:
        results.setdefault(p, (1, False))
    return results


def _last_commit_age(path: str) -> str:
    out = subprocess.run(
        ["git", "-C", path, "log", "-1", "--format=%cr"],
        capture_output=True,
        text=True,
    )
    return out.stdout.strip() or "unknown"


# --- fleet input ---------------------------------------------------------


def _load_registry() -> tuple[dict[str, str], bool]:
    """cwd -> status, plus an ok flag.

    A missing registry is a legitimate empty fleet (nothing has ever
    registered) and is ok. A registry that exists but fails to parse is a
    genuine read failure: reading it as empty would silently read every
    live worker as absent, which is exactly the false STRANDED constraint 4
    forbids."""
    override = os.environ.get("WORKTREE_STATUS_REGISTRY")
    target = Path(override) if override else Path(os.path.expanduser("~/.fno/agents/registry.json"))
    if not target.exists():
        return {}, True
    try:
        data = json.loads(target.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return {}, False
    if not isinstance(data, dict):
        return {}, False

    best: dict[str, tuple[int, str, str]] = {}
    for a in data.get("agents", []):
        cwd = a.get("cwd") or ""
        if not cwd:
            continue
        cwd = str(Path(cwd))
        status = a.get("status") or "unknown"
        rank = 1 if status in _ALIVE_STATUSES else 0
        ts = a.get("last_reconciled_at") or a.get("exited_at") or a.get("created_at") or ""
        cur = best.get(cwd)
        if cur is None or (rank, ts) > (cur[0], cur[1]):
            best[cwd] = (rank, ts, status)
    return {cwd: status for cwd, (_rank, _ts, status) in best.items()}, True


# --- the sweep driver ------------------------------------------------------


def _worktrees(repo: Path) -> list[tuple[Optional[str], str]]:
    """[(branch_or_None, path), ...] for every worktree, detached included."""
    out = subprocess.run(
        ["git", "-C", str(repo), "worktree", "list", "--porcelain"],
        capture_output=True,
        text=True,
    )
    rows: list[tuple[Optional[str], str]] = []
    wt_path = ""
    branch: Optional[str] = None
    detached = False
    for line in out.stdout.splitlines() + [""]:
        if line.startswith("worktree "):
            if wt_path:
                rows.append((None if detached else branch, wt_path))
            wt_path = line[len("worktree ") :]
            branch = None
            detached = False
        elif line.startswith("branch refs/heads/"):
            branch = line[len("branch refs/heads/") :]
        elif line == "detached":
            detached = True
    if wt_path:
        rows.append((None if detached else branch, wt_path))
    return rows


def sweep(repo: Optional[Path] = None) -> list[Row]:
    """Classify every worktree registered to ``repo`` (default: cwd's repo)."""
    repo = repo or Path(resolve_repo_root())
    worktrees = _worktrees(repo)
    paths = [p for _b, p in worktrees]

    unpushed_by_path = _unpushed_batch(paths)
    registry, registry_ok = _load_registry()

    try:
        entries = read_graph_strict()
        graph_ok = True
    except (GraphUnreadableError, GraphMalformedRootError):
        entries = []
        graph_ok = False
    entries_by_id = {e.get("id"): e for e in entries if isinstance(e, dict) and e.get("id")}

    rows: list[Row] = []
    for branch, path in worktrees:
        unpushed, unpushed_ok = unpushed_by_path.get(path, (1, False))
        node, node_entry = resolve_node_id(path, branch, entries_by_id)
        registry_status = registry.get(str(Path(path)))
        rows.append(
            classify(
                path=path,
                branch=branch,
                unpushed=unpushed,
                unpushed_ok=unpushed_ok,
                node=node,
                node_entry=node_entry,
                graph_ok=graph_ok,
                registry_status=registry_status,
                registry_ok=registry_ok,
                age=_last_commit_age(path),
            )
        )
    return rows
