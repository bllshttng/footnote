"""One serialized quota policy for every Footnote GraphQL caller."""
from __future__ import annotations

import fcntl
import json
import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Optional, Sequence

from fno.paths import github_cli_proxy_dir, graphql_quota_lock
from fno.pr._proc import Result, run

GRAPHQL_RESERVE = 200
REFUSED = 75


def quota_lock_path() -> Path:
    return graphql_quota_lock()


def resolve_real_gh() -> Optional[str]:
    configured = os.environ.get("FNO_REAL_GH")
    if configured:
        return configured
    proxy = (github_cli_proxy_dir() / "gh").resolve()
    for directory in os.environ.get("PATH", "").split(os.pathsep):
        if not directory:
            continue
        candidate = Path(directory) / "gh"
        if not candidate.is_file() or not os.access(candidate, os.X_OK):
            continue
        if candidate.resolve() != proxy:
            return str(candidate.resolve())
    return None


def _quota(payload: str) -> tuple[Optional[int], Optional[int]]:
    try:
        row = json.loads(payload)["resources"]["graphql"]
        remaining, reset = row["remaining"], row["reset"]
    except (json.JSONDecodeError, KeyError, TypeError):
        return None, None
    if not isinstance(remaining, int) or not isinstance(reset, int):
        return None, None
    return remaining, reset


def _pr_number(args: Sequence[str]) -> str:
    for index, arg in enumerate(args):
        if arg in {"view", "checks"} and index + 1 < len(args) and args[index + 1].isdigit():
            return args[index + 1]
    return "<n>"


def _refusal(args: Sequence[str], *, reset: Optional[int], unavailable: bool = False) -> str:
    pr = _pr_number(args)
    if unavailable:
        window = "quota instrument unavailable"
    elif reset is None:
        window = "the current quota window resets"
    else:
        stamp = datetime.fromtimestamp(reset, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        window = f"reset at {stamp}"
    return (
        f"GraphQL discretionary read refused: {window}. Use `fno pr info {pr}` for "
        f"state/head/mergeability and `fno pr status {pr}` for CI; stop retrying GraphQL "
        "until reset. `fno pr status` still contains optional review-thread and coverage reads "
        "that are GraphQL; those reads preserve the reserved coverage budget."
    )


def execute_graphql(
    purpose: str,
    gh_args: Sequence[str],
    *,
    runner: Callable = run,
    real_gh: Optional[str] = None,
    lock_path: Optional[Path] = None,
    cwd: Optional[str] = None,
    timeout: float = 120,
) -> Result:
    """Probe and execute under one machine-wide lock from probe through command."""
    if purpose not in {"discretionary", "coverage"}:
        return Result(2, "", "purpose must be discretionary or coverage")
    if not gh_args:
        return Result(2, "", "graphql-exec needs gh arguments after --")
    # A caller spelling bare ``gh`` inside a protected worker would resolve to
    # the proxy. Re-entering the broker while this process holds the flock
    # deadlocks until the outer command times out, so only an explicit non-bare
    # path bypasses the pinned-delegate resolver.
    gh = real_gh if real_gh and real_gh != "gh" else resolve_real_gh()
    if not gh:
        return Result(127, "", "gh not found on PATH")
    lock = lock_path or quota_lock_path()
    lock.parent.mkdir(parents=True, exist_ok=True)
    with lock.open("a+") as handle:
        fcntl.flock(handle, fcntl.LOCK_EX)
        try:
            probe = runner([gh, "api", "rate_limit"], cwd=cwd, timeout=min(30, timeout))
            remaining, reset = _quota(probe.stdout) if probe.ok else (None, None)
            if purpose == "discretionary":
                if remaining is None:
                    return Result(REFUSED, "", _refusal(gh_args, reset=None, unavailable=True))
                if remaining <= GRAPHQL_RESERVE:
                    return Result(REFUSED, "", _refusal(gh_args, reset=reset))
            return runner([gh, *gh_args], cwd=cwd, timeout=timeout)
        finally:
            fcntl.flock(handle, fcntl.LOCK_UN)
