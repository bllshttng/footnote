"""The three-way stranded-worktree classifier.

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

import importlib.util
import json
import logging
import re
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from fno.agents.registry import _OWNERSHIP_LIVE_STATUSES as _ALIVE_STATUSES
from fno.graph.fuzzy import resolve_node
from fno.graph.store import (
    GraphMalformedRootError,
    GraphUnreadableError,
    read_graph_strict,
)


def _load_worktree_status_module():
    """``scripts/lib/worktree-status.py`` as a module, loaded once. It is a
    standalone script (invoked by worktree-lifecycle.sh via subprocess), not
    part of the installed package, so it needs ``spec_from_file_location``
    rather than a normal import - the same porcelain parser and registry
    reader this file reuses used to be copied verbatim into this file, and a
    code review caught both copies going byte-for-byte/near-identical in
    this same PR, exactly the "N implementations of one operation" trap."""
    script = Path(__file__).resolve().parents[3] / "scripts" / "lib" / "worktree-status.py"
    spec = importlib.util.spec_from_file_location("_fno_worktree_status", script)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


_worktree_status_module = _load_worktree_status_module()
_worktrees = _worktree_status_module._worktrees

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

    if not (unpushed_ok and graph_ok and registry_ok):
        # Checked before "node unresolved": a graph read failure also empties
        # entries_by_id, so resolve_node_id necessarily returns (None, None)
        # too - reporting "node unresolved" in that case would hide the real,
        # more specific failure this event exists to surface.
        failed = [
            name
            for name, ok in (("git", unpushed_ok), ("graph", graph_ok), ("fleet", registry_ok))
            if not ok
        ]
        return Row(UNKNOWN, node, unpushed, age, {**facts, "reason": f"read failed: {','.join(failed)}"})

    if node is None or node_entry is None:
        return Row(UNKNOWN, node, unpushed, age, {**facts, "reason": "node unresolved"})

    # LIVE outranks every node-status read below it, not just PR_OPEN: a
    # code-review finding caught that a node auto-transitioning to "done" or
    # a terminal status (deferred/superseded) WHILE a worker is still
    # mid-commit in that worktree misclassified as SHIPPED/ABANDONED instead
    # of LIVE, and the hook then suggested `worktree cleanup --merged` on a
    # genuinely live session. The fleet leg is the only input that
    # distinguishes a live worker from a corpse; nothing about node status
    # ever overrides it.
    if registry_status in _ALIVE_STATUSES:
        return Row(LIVE, node, unpushed, age, facts)

    if node_entry.get("status") == "done":
        return Row(SHIPPED, node, unpushed, age, facts)

    if node_entry.get("status") in _QUIET_TERMINAL_STATUSES:
        return Row(ABANDONED, node, unpushed, age, facts)

    if node_entry.get("pr_number"):
        return Row(PR_OPEN, node, unpushed, age, facts)

    return Row(STRANDED, node, unpushed, age, facts)


# --- node resolution ---------------------------------------------------


def _basename_candidate(path: str) -> str:
    return Path(path).name


def _branch_candidate(branch: Optional[str]) -> Optional[str]:
    """Last ``/``-delimited segment: `feature/x-ab12` -> `x-ab12`."""
    if not branch:
        return None
    return branch.rsplit("/", 1)[-1]


def _read_state_graph_node_id(path: str) -> Optional[str]:
    """The ``graph_node_id: <id>`` line appended to the manifest BODY by
    ``hooks/helpers/init-target-state.sh`` - outside the YAML frontmatter,
    so ``fno do state show --field`` cannot return it."""
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


def _unpushed_batch(paths: list[str]) -> dict[str, tuple[int, bool, str]]:
    """path -> (unpushed_count, ok, age). Shells to the shared
    ``wt_unpushed_count`` (scripts/lib/worktree-unpushed.sh) rather than a
    second implementation of its fail-toward-keep contract. All paths run in
    one bash process so the script's own per-process fetch cache (exported
    ``_WT_REMOTE_REFS_FRESH``/``_STALE``) verifies the remote exactly once
    for the whole batch, not once per worktree - and the last-commit age
    rides the same loop iteration rather than a second subprocess per path.

    The script's own path is resolved from this FILE's location, never
    ``resolve_repo_root()``: that helper reads the caller's ambient cwd (or
    a process-wide cached env var) and has no idea which of possibly many
    swept repos is in play, so a multi-repo sweep - or a daemon started
    with no ambient cwd at all - could resolve the wrong repo, or none, for
    every root after whichever one happened to be cached first. This
    script always lives at a fixed offset from this module regardless of
    which worktree's commits are being counted."""
    if not paths:
        return {}
    script = Path(__file__).resolve().parents[3] / "scripts" / "lib" / "worktree-unpushed.sh"
    driver = (
        'source "$1"; shift\n'
        'for p in "$@"; do\n'
        '  err="$(mktemp)"\n'
        '  out="$(wt_unpushed_count "$p" 2>"$err")"\n'
        '  errtext="$(cat "$err")"; rm -f "$err"\n'
        '  ok=1\n'
        '  case "$errtext" in *"not verifiable"*) ok=0 ;; esac\n'
        '  age="$(git -C "$p" log -1 --format=%cr 2>/dev/null)"\n'
        "  printf '%s\\x1f%s\\x1f%s\\x1f%s\\n' \"$p\" \"$out\" \"$ok\" \"$age\"\n"
        "done\n"
    )
    proc = subprocess.run(
        ["bash", "-c", driver, "bash", str(script), *paths],
        capture_output=True,
        text=True,
    )
    results: dict[str, tuple[int, bool, str]] = {}
    for line in (proc.stdout or "").splitlines():
        parts = line.split("\x1f")
        if len(parts) != 4:
            continue
        p, count_s, ok_s, age = parts
        count = int(count_s) if count_s.isdigit() else 1
        results[p] = (count, ok_s == "1", age or "unknown")
    # A path the driver never reported (bash itself failed) fails toward
    # "unpushed and unverifiable", the same posture wt_unpushed_count takes.
    for p in paths:
        results.setdefault(p, (1, False, "unknown"))
    return results


# --- fleet input ---------------------------------------------------------


def _load_registry() -> tuple[dict[str, str], bool]:
    """cwd -> status, plus an ok flag.

    Delegates to ``scripts/lib/worktree-status.py``'s own ``_load_registry``
    (loaded once as ``_worktree_status_module`` below) rather than a second
    copy of the same best-row selection - the exact "N implementations of
    one operation" trap ``_worktrees`` below was already fixed for. That
    function returns cwd -> (name, status); this drops the name, which only
    the display CLI needs.

    The ok flag matters here in a way it does not for a display tool: a
    missing registry is a legitimate empty fleet (nothing has ever
    registered) and is ok, but a registry that exists and fails to parse is
    a genuine read failure - reading it as empty would silently read every
    live worker as absent, which is exactly the false STRANDED constraint 4
    forbids."""
    by_cwd, ok = _worktree_status_module._load_registry()
    return {cwd: status for cwd, (_name, status) in by_cwd.items()}, ok


# --- the sweep driver ------------------------------------------------------


def sweep(repo: Path) -> list[Row]:
    """Classify every worktree registered to ``repo``.

    ``repo`` resolution is the caller's job, not this module's: a module
    that calls the shared ``resolve_repo_root()`` itself would drag every
    ``scripts/``-relative path in this file (the ``_unpushed_batch`` /
    ``_worktrees`` script lookups, deliberately package-relative via
    ``Path(__file__)`` rather than that same resolver - see their
    docstrings) into `fno doctor lint shellout-drift`'s scan scope for no reason;
    that guard flags a module on the mere co-occurrence of a bash-exec and
    a resolver call, not on which one actually roots the script path.
    """
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
        unpushed, unpushed_ok, age = unpushed_by_path.get(path, (1, False, "unknown"))
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
                age=age,
            )
        )
    return rows


# --- recovery acts (STRANDED only) + UNKNOWN recording --------------------
#
# One producer for both call sites (the `stranded --apply` verb and the
# pr-watch tick leg): a guard or an act living on only one of two reachable
# paths is decorative on the path it skips, so both route through the same
# functions here rather than each re-implementing "push, file, emit".


def act_on_stranded(row: Row) -> dict:
    """The three non-destructive acts, in order, stopping at the first
    failure. Caller's responsibility to only call this for a STRANDED row."""
    path = row.facts["path"]
    branch = row.facts.get("branch")
    node = row.node
    assert node is not None, "act_on_stranded requires a STRANDED row, which always has a resolved node"
    acts: list[dict] = []

    sha_p = subprocess.run(["git", "-C", path, "rev-parse", "HEAD"], capture_output=True, text=True)
    sha = (sha_p.stdout or "").strip()

    if branch:
        push_branch = branch
        push_p = subprocess.run(
            ["git", "-C", path, "push", "-u", "origin", branch], capture_output=True, text=True
        )
    else:
        push_branch = f"recovered/{node}"
        push_p = subprocess.run(
            ["git", "-C", path, "push", "origin", f"HEAD:refs/heads/{push_branch}"],
            capture_output=True,
            text=True,
        )
    push_ok = push_p.returncode == 0
    acts.append({"act": "push", "branch": push_branch, "ok": push_ok, "detail": (push_p.stderr or "").strip()[:500]})
    if not push_ok:
        return {"node": node, "class": row.klass, "acts": acts, "stopped_at": "push"}

    detail_line = (
        f"Recovered {row.unpushed} unpushed commit(s) at {sha[:12] or 'unknown'} "
        f"onto {push_branch} (stranded sweep)."
    )
    get_p = subprocess.run(["fno", "backlog", "get", node], capture_output=True, text=True)
    try:
        cur_details = (json.loads(get_p.stdout or "{}").get("details") or "") if get_p.returncode == 0 else ""
    except json.JSONDecodeError:
        cur_details = ""
    new_details = f"{cur_details}\n\n{detail_line}" if cur_details else detail_line
    upd_p = subprocess.run(
        ["fno", "backlog", "update", node, "--details", new_details], capture_output=True, text=True
    )
    upd_ok = upd_p.returncode == 0
    acts.append({"act": "backlog_update", "ok": upd_ok, "detail": (upd_p.stderr or "").strip()[:500]})
    if not upd_ok:
        return {"node": node, "class": row.klass, "acts": acts, "stopped_at": "backlog_update"}

    ev_ok = _emit_sweep_event(row, node=node, branch=push_branch, sha=sha, acts=[a["act"] for a in acts])
    acts.append({"act": "event_emit", "ok": ev_ok})
    return {"node": node, "class": row.klass, "acts": acts, "stopped_at": None if ev_ok else "event_emit"}


def record_unknown(row: Row) -> dict:
    """UNKNOWN rows: recorded in the event, never pushed, never filed."""
    ev_ok = _emit_sweep_event(row, node=row.node, branch=row.facts.get("branch"), sha=None, acts=[])
    return {
        "node": row.node,
        "class": row.klass,
        "acts": [{"act": "event_emit", "ok": ev_ok}],
        "stopped_at": None if ev_ok else "event_emit",
    }


def _emit_sweep_event(
    row: Row, *, node: Optional[str], branch: Optional[str], sha: Optional[str], acts: list[str]
) -> bool:
    """In-process, not a shell to `fno doctor event emit`: the same pattern
    fno.agents.watchdog.emit_event already uses for this daemon's sibling
    fleet-watchdog leg. A miss is swallowed loudly (a log warning, not
    silence) so a whole lane of telemetry going missing is never mistaken
    for a clean run, but it still never breaks the sweep. Also fixes a bug
    the subprocess form had regardless of process cost: `stranded_sweep`
    was never registered in events/schema.yaml, so every emit here was
    silently failing validation - and `--node` on the CLI form is a no-op
    for any non-x-dbaf-family type (envelope stays None), so `node` goes in
    `data` instead, where it is actually stored."""
    data = {
        "path": row.facts.get("path"),
        "branch": branch,
        "class": row.klass,
        "unpushed": row.unpushed,
        "sha": sha,
        "acts": acts,
        "reason": row.facts.get("reason"),
        "node": node,
    }
    try:
        from fno import paths as _paths
        from fno.events import _build, append_event

        append_event(_build("stranded_sweep", "daemon", data), _paths.state_dir() / "events.jsonl")
        return True
    except Exception as exc:  # noqa: BLE001 - telemetry must never break the sweep
        logging.getLogger(__name__).warning(
            "worktree_stranded: event stranded_sweep not written: %s", exc
        )
        return False


def apply_sweep(rows: list[Row], *, wake: bool = True) -> list[dict]:
    """Act on a classified sweep: STRANDED rows get pushed and filed,
    UNKNOWN rows get recorded, every other class stays quiet. Stops at the
    first failed act per row; a later tick retries that row from scratch.

    ``wake=False`` (the fleet watchdog's "report" posture) still records
    every UNKNOWN row - recording is reporting, not acting, per this
    module's own fail-open contract - but skips the STRANDED push+file, the
    one genuinely mutating act here. The explicit ``fno worktree stranded
    --apply`` verb is a direct user request, not a watchdog-mode read, so
    it keeps the wake=True default and always acts."""
    outcomes: list[dict] = []
    for row in rows:
        if row.klass == STRANDED:
            if wake:
                outcomes.append(act_on_stranded(row))
        elif row.klass == UNKNOWN:
            outcomes.append(record_unknown(row))
    return outcomes
