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
from fno.setup.github_cli import PROXY_DEPTH_ENV, PROXY_EXEC_LINE

GRAPHQL_RESERVE = 200
REFUSED = 75
_PROXY_DIR_ENV = "FNO_GH_PROXY_DIR"


class ProxyIdentityError(RuntimeError):
    """The proxy cannot work out which directories its own shims live in."""


def proxy_identity_refusal(exc: BaseException) -> str:
    """One wording for the refusal, so the four callers cannot drift apart."""
    return (
        "gh proxy: cannot identify its own install directory, refusing to "
        f"delegate: {exc}"
    )


def quota_lock_path() -> Path:
    return graphql_quota_lock()


_SHIM_SCAN_BYTES = 65536


def _is_proxy_shim(path: Path) -> bool:
    """Is this ``gh`` our own broker shim, judged by content rather than location?

    ``github_cli_proxy_dir()`` is TMPDIR-derived, so directory identity only
    recognizes a shim written by a process sharing our TMPDIR. A background job
    or a launchd agent inherits the shim on PATH but computes a different
    directory, and so fails to recognize it. The shim is a two-line script, but
    a wrapper generator may pad it with extra lines before or after the exec
    line, so the read is bounded, not gated on total size: ``_SHIM_SCAN_BYTES``
    comfortably covers any realistic wrapper while staying far below a real
    compiled ``gh`` binary. The match is anchored to the exec line
    ``ensure_proxy`` writes (``PROXY_EXEC_LINE``) rather than a bare substring,
    so a real ``gh`` that merely mentions the proxy name in a comment is not
    mistaken for the shim.
    """
    try:
        with path.open("rb") as handle:
            head = handle.read(_SHIM_SCAN_BYTES)
    except OSError:
        return False
    text = head.decode("utf-8", errors="ignore")
    return any(line.strip() == PROXY_EXEC_LINE for line in text.splitlines())


def _proxy_dirs() -> set[str]:
    """Every directory one of our own shims can sit in.

    Swallowing the lookup failure returned an empty set, and an empty set says
    two different things: "there is no proxy" and "I could not find out". Every
    caller read it as the first. `resolve_real_gh` then handed back the proxy's
    own shim and `os.execve` re-entered it with the same argv forever. So this
    fails closed: a proxy that cannot identify itself must not delegate,
    because calling itself is the one thing it is able to do. An inherited
    ``FNO_GH_PROXY_DIR`` already answers the question, so a config load that
    fails after that is not an identity failure and must not refuse every gh
    command in the worker.
    """
    paths = set()
    inherited = os.environ.get(_PROXY_DIR_ENV)
    if inherited:
        paths.add(str(Path(inherited).resolve()))
    try:
        paths.add(str(github_cli_proxy_dir().resolve()))
    except Exception as exc:
        if not paths:
            raise ProxyIdentityError(str(exc)) from exc
    return paths


def delegate_environment() -> dict[str, str]:
    """Remove the quota proxy from PATH before invoking a preserved gh wrapper."""
    env = dict(os.environ)
    proxy_dirs = _proxy_dirs()
    env["PATH"] = os.pathsep.join(
        part for part in env.get("PATH", "").split(os.pathsep)
        if part
        and str(Path(part).resolve()) not in proxy_dirs
        and not _is_proxy_shim(Path(part) / "gh")
    )
    env.pop("FNO_REAL_GH", None)
    env.pop(_PROXY_DIR_ENV, None)
    # `delegate` re-stamps this immediately for its own execve. Every other
    # caller runs the real gh directly, and its descendants must not inherit a
    # marker that makes a later first entry look like a loop.
    env.pop(PROXY_DEPTH_ENV, None)
    return env


def resolve_real_gh() -> Optional[str]:
    configured = os.environ.get("FNO_REAL_GH")
    proxy_dirs = _proxy_dirs()
    if configured:
        candidate = Path(configured)
        if (
            candidate.is_file()
            and os.access(candidate, os.X_OK)
            and str(candidate.parent.resolve()) not in proxy_dirs
            and not _is_proxy_shim(candidate)
        ):
            return str(candidate.resolve())
    for directory in os.environ.get("PATH", "").split(os.pathsep):
        if not directory:
            continue
        candidate = Path(directory) / "gh"
        if not candidate.is_file() or not os.access(candidate, os.X_OK):
            continue
        if str(candidate.parent.resolve()) in proxy_dirs or _is_proxy_shim(candidate):
            continue
        # Always absolute, never the bare name. `delegate` execve's this and
        # execve does no PATH lookup, so a bare name raises FileNotFoundError.
        # `execute_graphql` runs it while holding the flock, so a bare name
        # re-enters the shim and blocks until the outer command times out.
        return str(candidate.resolve())
    return None


def _coverage_read(args: Sequence[str]) -> bool:
    """Only review-coverage computation and publication reads spend the reserve."""
    if len(args) < 4 or list(args[:2]) != ["pr", "view"]:
        return False
    try:
        fields_arg = args[args.index("--json") + 1]
    except (ValueError, IndexError):
        fields_arg = next(
            (arg.partition("=")[2] for arg in args if arg.startswith("--json=")), ""
        )
    fields = frozenset(part for part in fields_arg.split(",") if part)
    return fields in {
        frozenset({"reviews", "comments"}),
        frozenset({"reviews", "comments", "headRefOid", "baseRefName"}),
        frozenset({"commits"}),
        frozenset({"labels"}),
    }


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


def _malformed_argv_refusal(first: str) -> str:
    """Teach the shape at the point a flag-first argv would exec into noise.

    Everything after ``--`` becomes gh's argv, command words first. Flags with
    no command word make gh print its root command list, which reads as
    unrelated output rather than an error, so the caller cannot tell the route
    failed (a king lost a merge-precondition read this way).
    """
    return (
        "graphql-exec passes everything after -- to gh as its full argv, "
        f"command words first; an argv starting with '{first}' is flags with no "
        "command, which gh answers with its root command list. Route the "
        "refused call as: fno do pr graphql-exec --purpose discretionary -- "
        "api graphql -f query=... --jq ..."
    )


def _refusal(args: Sequence[str], *, reset: Optional[int], unavailable: bool = False) -> str:
    pr = _pr_number(args)
    if unavailable:
        window = "quota instrument unavailable"
    elif reset is None:
        window = "the current quota window resets"
    else:
        stamp = datetime.fromtimestamp(reset, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        window = f"reset at {stamp}"
    if len(args) >= 2 and list(args[:2]) == ["pr", "list"]:
        cheap = "Use `fno do pr list` for a REST-backed PR listing"
    else:
        cheap = (
            f"Use `fno do pr info {pr}` for state/head/mergeability and "
            f"`fno do pr status {pr}` for CI"
        )
    return (
        f"GraphQL discretionary read refused: {window}. {cheap}; stop retrying GraphQL "
        "until reset. `fno do pr status` still contains optional review-thread and coverage reads "
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
    if gh_args[0].startswith("-"):
        return Result(2, "", _malformed_argv_refusal(gh_args[0]))
    if purpose == "coverage" and not _coverage_read(gh_args):
        return Result(2, "", "coverage reserve accepts review-coverage reads only")
    # A caller spelling bare ``gh`` inside a protected worker would resolve to
    # the proxy. Re-entering the broker while this process holds the flock
    # deadlocks until the outer command times out, so only an explicit non-bare
    # path bypasses the pinned-delegate resolver.
    try:
        gh = real_gh if real_gh and real_gh != "gh" else resolve_real_gh()
    except ProxyIdentityError as exc:
        return Result(2, "", proxy_identity_refusal(exc))
    if not gh:
        return Result(127, "", "gh not found on PATH")
    lock = lock_path or quota_lock_path()
    lock.parent.mkdir(parents=True, exist_ok=True)
    with lock.open("a+") as handle:
        fcntl.flock(handle, fcntl.LOCK_EX)
        try:
            try:
                env = delegate_environment()
            except ProxyIdentityError as exc:
                return Result(2, "", proxy_identity_refusal(exc))
            probe = runner(
                [gh, "api", "rate_limit"], cwd=cwd, timeout=min(30, timeout), env=env
            )
            remaining, reset = _quota(probe.stdout) if probe.ok else (None, None)
            if purpose == "discretionary":
                if remaining is None:
                    return Result(REFUSED, "", _refusal(gh_args, reset=None, unavailable=True))
                if remaining <= GRAPHQL_RESERVE:
                    return Result(REFUSED, "", _refusal(gh_args, reset=reset))
            return runner([gh, *gh_args], cwd=cwd, timeout=timeout, env=env)
        finally:
            fcntl.flock(handle, fcntl.LOCK_UN)
