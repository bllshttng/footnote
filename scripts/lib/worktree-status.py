#!/usr/bin/env python3
"""List a repo's git worktrees annotated with their real session, if any.

Cross-references each worktree path against ~/.fno/agents/registry.json's
`cwd` field - the live agents registry the daemon already reconciles - rather
than reading `.fno/target-state.md`'s `owner_pid`, which names the short-lived
`fno target init` CLI invocation and reads as dead within seconds of session
start (verified live 2026-08-15: a worktree with an active session showed
owner_pid already exited). The registry's `status` field (spawning/ready/
idle/busy/live/restarting/orphaned/failed/exited/permanent_dead) is itself
computed by measurement (the Rust daemon's reconciliation sweep), so this is
a read, not a second liveness probe.

Usage: worktree-status.py [--json] [--repo <path>]
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Optional

# Non-terminal AgentStatus vocabulary (crates/fno-agents/src/lib.rs
# `AgentStatus`), mirrored from `_OWNERSHIP_LIVE_STATUSES` in
# cli/src/fno/agents/registry.py: a row in any of these is still an active
# session, not just the literal "live" wire value - most rows sit in "idle"
# or "busy", not "live", so matching on "live" alone reads a working session
# as dead.
_ALIVE_STATUSES = frozenset({"spawning", "ready", "idle", "busy", "live", "restarting"})


def _load_registry() -> tuple[dict[str, tuple[str, str]], bool]:
    """cwd -> (name, status), preferring a live row, else the most recent.

    Plus an ok flag: a missing registry is a legitimate empty fleet (nothing
    has ever registered) and is ok; a registry that exists but fails to
    parse is a genuine read failure. worktree_stranded.py's fail-open
    classifier reuses this function (rather than a second copy of the same
    best-row selection) and needs that distinction - reading a corrupt
    registry as empty would silently read every live worker as absent."""
    override = os.environ.get("WORKTREE_STATUS_REGISTRY")
    path = Path(override) if override else Path(os.path.expanduser("~/.fno/agents/registry.json"))
    if not path.exists():
        return {}, True
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return {}, False
    if not isinstance(data, dict):
        return {}, False
    best: dict[str, tuple[int, str, str, str]] = {}
    for a in data.get("agents", []):
        cwd = a.get("cwd") or ""
        if not cwd:
            continue
        cwd = str(Path(cwd))
        status = a.get("status") or "unknown"
        rank = 1 if status in _ALIVE_STATUSES else 0
        ts = a.get("last_reconciled_at") or a.get("exited_at") or a.get("created_at") or ""
        name = a.get("name") or "?"
        cur = best.get(cwd)
        if cur is None or (rank, ts) > (cur[0], cur[2]):
            best[cwd] = (rank, name, ts, status)
    return {cwd: (name, status) for cwd, (_rank, name, _ts, status) in best.items()}, True


def _worktrees(repo: Path) -> list[tuple[Optional[str], str]]:
    """[(branch, path), ...] for every worktree registered to `repo`.

    A row only appended on a `branch refs/heads/` line drops every detached
    worktree from the output - three of the previously reported stranded
    rows were detached, so the surface structurally could not show the
    cases that matter most. Emit those too, with `branch: None`.

    A `bare` entry (the main admin directory of a bare-repo-as-worktree-
    container setup) carries neither a `branch` nor a `detached` line, so
    without an explicit check it would fall through the same as a real
    detached worktree and misreport as one. It has no working tree to
    inspect, so it is excluded entirely - the same as the pre-fix behavior,
    which also never appended a row for it (a `bare` entry has no `branch
    refs/heads/` line either).
    """
    out = subprocess.run(
        ["git", "-C", str(repo), "worktree", "list", "--porcelain"],
        capture_output=True,
        text=True,
    )
    rows: list[tuple[Optional[str], str]] = []
    wt_path = ""
    branch: Optional[str] = None
    detached = False
    bare = False
    for line in out.stdout.splitlines():
        if line.startswith("worktree "):
            if wt_path and not bare:
                rows.append((None if detached else branch, wt_path))
            wt_path = line[len("worktree ") :]
            branch = None
            detached = False
            bare = False
        elif line.startswith("branch refs/heads/"):
            branch = line[len("branch refs/heads/") :]
        elif line == "detached":
            detached = True
        elif line == "bare":
            bare = True
    if wt_path and not bare:
        rows.append((None if detached else branch, wt_path))
    return rows


def _last_commit_age(path: str) -> str:
    out = subprocess.run(
        ["git", "-C", path, "log", "-1", "--format=%cr"],
        capture_output=True,
        text=True,
    )
    return out.stdout.strip() or "unknown"


def main(argv: list[str]) -> int:
    as_json = "--json" in argv
    repo = Path.cwd()
    if "--repo" in argv:
        repo = Path(argv[argv.index("--repo") + 1])

    registry, registry_ok = _load_registry()
    if not registry_ok:
        print("worktree-status: registry.json exists but could not be parsed; "
              "every session reads as none until it is fixed", file=sys.stderr)
    rows = []
    total = live_n = dead_n = none_n = 0
    for branch, wt_path in _worktrees(repo):
        name, status = registry.get(str(Path(wt_path)), ("", ""))
        if not name:
            target = "none"
            none_n += 1
        elif status in _ALIVE_STATUSES:
            target = f"live:{name}"
            live_n += 1
        else:
            target = f"{status or 'exited'}:{name}"
            dead_n += 1
        total += 1
        rows.append(
            {
                "branch": branch,
                "path": wt_path,
                "last_commit": _last_commit_age(wt_path),
                "target": target,
                "session_name": name,
            }
        )

    if as_json:
        print(
            json.dumps(
                {
                    "worktrees": rows,
                    "summary": {
                        "total": total,
                        "live": live_n,
                        "dead": dead_n,
                        "no_session": none_n,
                    },
                },
                separators=(",", ":"),
            )
        )
        return 0

    print("Worktrees:")
    for r in rows:
        branch_label = r["branch"] or "(detached)"
        print(
            f"  {branch_label:<30} | {r['last_commit']:<15} | "
            f"target: {r['target']:<20} | {r['path']}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
