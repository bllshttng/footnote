"""List Git worktrees annotated with their registered agent session.

Joins each worktree path on the agents registry's `cwd` field, never
`.fno/target-state.md`'s `owner_pid` (the short-lived init CLI's pid, dead
within seconds of session start); the registry `status` is a measurement the
daemon already computed, so this is a read, not a second probe. Packaged
outside scripts/lib so an installed wheel keeps the behavior.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Optional


# Non-terminal AgentStatus vocabulary, one definition: importing the
# canonical set (crates/fno-agents `AgentStatus` mirror) keeps the display
# leg and the stranded classifier agreeing when the vocabulary grows.
from fno.agents.registry import _OWNERSHIP_LIVE_STATUSES as _ALIVE_STATUSES
from fno.agents.registry import registry_rows_by_cwd


def _load_registry() -> tuple[dict[str, tuple[str, str]], bool]:
    """Return cwd -> (name, status), preferring a live row, plus read status.

    Builds on the ONE shared occupancy join (x-dead task 0.2); the best-row
    selection below (live row outranks, then freshest timestamp) stays here
    because only the display leg needs it."""
    rows, ok = registry_rows_by_cwd()
    if not ok:
        return {}, False
    best: dict[str, tuple[int, str, str, str]] = {}
    for cwd, agents in rows.items():
        for agent in agents:
            status = agent.get("status") or "unknown"
            rank = 1 if status in _ALIVE_STATUSES else 0
            timestamp = (
                agent.get("last_reconciled_at")
                or agent.get("exited_at")
                or agent.get("created_at")
                or ""
            )
            name = agent.get("name") or "?"
            current = best.get(cwd)
            if current is None or (rank, timestamp) > (current[0], current[2]):
                best[cwd] = (rank, name, timestamp, status)
    return {
        cwd: (name, status)
        for cwd, (_rank, name, _timestamp, status) in best.items()
    }, True


def _worktrees(repo: Path) -> list[tuple[Optional[str], str]]:
    """Return ``(branch, path)`` for attached and detached worktrees."""
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
        i = argv.index("--repo")
        if i + 1 >= len(argv):
            print("worktree-status: --repo requires a value", file=sys.stderr)
            return 2
        repo = Path(argv[i + 1])

    registry, registry_ok = _load_registry()
    if not registry_ok:
        print(
            "worktree-status: registry.json exists but could not be parsed; "
            "every session reads as none until it is fixed",
            file=sys.stderr,
        )
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
    for row in rows:
        branch_label = row["branch"] or "(detached)"
        print(
            f"  {branch_label:<30} | {row['last_commit']:<15} | "
            f"target: {row['target']:<20} | {row['path']}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
