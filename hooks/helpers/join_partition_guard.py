"""Judge one Edit/Write payload against a joined worktree's per-band policy.

Spawned by ``hooks/join-partition-write-guard.sh`` only after its stat fast
path confirmed ``<cwd>/.fno/join-partition/`` exists, so everything here is
already inside a joined worktree. Prints exactly one decision JSON (approve
``{}`` or a block) and always exits 0: a crash here must read as "not jailed"
(LD3), never as a wedge.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

_PATCH_PREFIXES = (
    "*** Add File: ",
    "*** Update File: ",
    "*** Delete File: ",
    "*** Move to: ",
)


def _approve() -> None:
    print("{}")
    sys.exit(0)


def _block(reason: str) -> None:
    print(
        json.dumps(
            {
                "decision": "block",
                "reason": reason,
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                },
            }
        )
    )
    sys.exit(0)


def _targets(tool_input: dict) -> list[str]:
    targets: list[str] = []
    file_path = tool_input.get("file_path")
    if isinstance(file_path, str) and file_path:
        targets.append(file_path)
    command = tool_input.get("command")
    if isinstance(command, str) and command:
        for line in command.splitlines():
            for prefix in _PATCH_PREFIXES:
                if line.startswith(prefix):
                    target = line[len(prefix) :].strip().rstrip("\r")
                    if target:
                        targets.append(target)
    return targets


def _resolve_worker(policy_dir: Path, session_id: str) -> str:
    """FNO_WORKER_NAME first, then the roster binding, cached per session.

    The cache is the cost discipline: the resolver walks the process tree, so
    a joined worktree pays it once per session, not once per Edit. A ``-``
    entry caches the unresolvable verdict so a session that cannot prove a
    name stops re-paying the walk.
    """
    name = (os.environ.get("FNO_WORKER_NAME") or "").strip()
    if name:
        return name
    cache = policy_dir / f".session-{session_id}" if session_id else None
    if cache is not None:
        try:
            cached = cache.read_text().strip()
            return "" if cached == "-" else cached
        except OSError:
            pass
    try:
        from fno.claims.self_identity import resolve_task_holder

        resolved, _refusal = resolve_task_holder()
        name = resolved or ""
    except Exception:  # noqa: BLE001 - identity trouble is never a jail
        name = ""
    if cache is not None:
        try:
            cache.write_text(name or "-")
        except OSError:
            pass
    return name


def main() -> None:
    try:
        payload = json.load(sys.stdin)
    except ValueError:
        _approve()
    if not isinstance(payload, dict):
        _approve()
    cwd = payload.get("cwd")
    if not isinstance(cwd, str) or not cwd:
        _approve()
    policy_dir = Path(cwd) / ".fno" / "join-partition"
    # Re-checked: the bash stat may have raced a join teardown.
    if not policy_dir.is_dir():
        _approve()
    targets = _targets(payload.get("tool_input") or {})
    if not targets:
        _approve()

    session_id = payload.get("session_id")
    session_id = session_id if isinstance(session_id, str) else ""
    name = _resolve_worker(policy_dir, session_id)
    policy_path = policy_dir / f"{name}.json" if name else None
    policy = None
    if policy_path is not None and policy_path.is_file():
        try:
            loaded = json.loads(policy_path.read_text())
            policy = loaded if isinstance(loaded, dict) else None
        except (OSError, ValueError):
            policy = None
    if not policy:
        # No policy for this name: the holder (LD3) or an unattributed
        # session - never jailed at this layer.
        _approve()
    deny_edit = [d for d in policy.get("deny_edit") or [] if isinstance(d, str)]
    if not deny_edit:
        _approve()
    band = policy.get("band") or "your band"
    base = Path(cwd).resolve()

    def owner_band(rel: str) -> str:
        for sibling in policy_dir.glob("*.json"):
            if sibling.name == f"{name}.json":
                continue
            try:
                other = json.loads(sibling.read_text())
            except (OSError, ValueError):
                continue
            allow = other.get("allow_write")
            if isinstance(allow, list) and rel in allow:
                return str(other.get("band") or "another")
        return "another"

    denied: list[tuple[str, str]] = []
    for raw in targets:
        path = Path(raw)
        phys = path if path.is_absolute() else base / path
        try:
            phys = phys.resolve()
        except OSError:
            # A target that will not resolve physically is unknown, not safe.
            denied.append((str(raw), band))
            continue
        try:
            rel = phys.relative_to(base).as_posix()
        except ValueError:
            # Outside the worktree entirely: not any band's surface, so the
            # partition has no verdict here (the OS layer owns that axis).
            continue
        hit = next(
            (
                d
                for d in deny_edit
                if rel == d.rstrip("/") or rel.startswith(d.rstrip("/") + "/")
            ),
            None,
        )
        if hit:
            denied.append((rel, owner_band(rel)))

    if denied:
        rel, owner = denied[0]
        _block(
            f"join partition: '{rel}' belongs to band '{owner}', not your band "
            f"'{band}'. Edit only your own band's files; a Bash write there is "
            "refused by the sandbox too."
        )
    _approve()


if __name__ == "__main__":
    try:
        main()
    except Exception:  # noqa: BLE001 - never wedge a session on a guard
        _approve()
