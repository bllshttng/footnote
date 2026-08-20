"""List Git worktrees annotated with their registered agent session."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Optional


_ALIVE_STATUSES = frozenset({"spawning", "ready", "idle", "busy", "live", "restarting"})


def _load_registry() -> tuple[dict[str, tuple[str, str]], bool]:
    """Return cwd -> (name, status), preferring a live row, plus read status."""
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
    for agent in data.get("agents", []):
        cwd = agent.get("cwd") or ""
        if not cwd:
            continue
        cwd = str(Path(cwd))
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
        repo = Path(argv[argv.index("--repo") + 1])

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
