"""Probe a bounded codex worker's sandbox before it is dispatched.

A codex worker in the ``workspace-write`` sandbox can launch, report ready,
and then wait forever on a tool the sandbox blocks: ``gh`` with no network, or
a git ref lock with no writable git dir. ``codex sandbox`` runs a command under
the same seatbelt policy and ``config.toml`` a worker gets, so each tool is
checked from inside it against a marker only that tool's success produces.

Two controls keep a refusal honest. A sandboxed echo must return its nonce, or
the probe never ran. A tool that fails inside the sandbox is run again outside
it: only a tool that answers outside and fails inside is blocked BY the
sandbox. A tool that fails in both places (gh logged out, GitHub down) says
nothing about the sandbox, so it reads ``unknown`` rather than refusing.
"""
from __future__ import annotations

import secrets
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Literal, Optional

EXIT_SANDBOX_UNREACHABLE = 82

_TIMEOUT_SECS = 15
_ERRORS = (OSError, subprocess.SubprocessError)


@dataclass(frozen=True)
class SandboxProbe:
    verdict: Literal["reachable", "blocked", "unknown"]
    blocked: list[tuple[str, str]] = field(default_factory=list)
    note: str = ""


def _first_line(text: str) -> str:
    # gh states its cause at the start of the first line; git's lock error states
    # it at the end, after a long path. A long line keeps both ends.
    lines = [line.strip() for line in (text or "").splitlines() if line.strip()]
    if not lines:
        return ""
    line = lines[0]
    return line if len(line) <= 160 else f"{line[:60]}...{line[-97:]}"


def _why(proc: "subprocess.CompletedProcess[str]") -> str:
    return _first_line(proc.stderr) or f"exit {proc.returncode}"


def probe_codex_sandbox(
    cwd: Path,
    *,
    run: Callable[..., "subprocess.CompletedProcess[str]"] = subprocess.run,
) -> SandboxProbe:
    """Check that ``gh`` and a git ref lock work inside the worker's sandbox."""
    from fno.agents.harnesses.codex import git_writable_config_args

    # The same roots override the resume lane grants, so the probe runs under
    # the grant the worker gets rather than a narrower default.
    sandbox = [
        "codex", "sandbox", "-c", 'sandbox_mode="workspace-write"',
        *git_writable_config_args(cwd), "--",
    ]

    def call(
        argv: list[str], *, sandboxed: bool = True, stdin: Optional[str] = None
    ) -> "subprocess.CompletedProcess[str]":
        return run(
            [*sandbox, *argv] if sandboxed else argv,
            cwd=str(cwd),
            input=stdin,
            capture_output=True,
            text=True,
            timeout=_TIMEOUT_SECS,
        )

    nonce = secrets.token_hex(8)
    try:
        control = call(["/bin/echo", nonce])
    except _ERRORS as exc:
        return SandboxProbe("unknown", note=f"{type(exc).__name__}: {exc}"[:160])
    if control.returncode != 0 or control.stdout.strip() != nonce:
        return SandboxProbe("unknown", note=_why(control))

    blocked: list[tuple[str, str]] = []
    unjudged: list[str] = []

    def judge(tool: str, argv: list[str], answered, stdin: Optional[str] = None) -> None:
        try:
            inside = call(argv, stdin=stdin)
            if answered(inside):
                return
            outside = call(argv, sandboxed=False, stdin=stdin)
            if answered(outside):
                blocked.append((tool, _why(inside)))
            else:
                unjudged.append(f"{tool} fails outside the sandbox too ({_why(outside)})")
        except _ERRORS as exc:
            unjudged.append(f"{tool}: {type(exc).__name__}: {exc}"[:160])

    def gh_answers(proc: "subprocess.CompletedProcess[str]") -> bool:
        limit = proc.stdout.strip()
        return proc.returncode == 0 and limit.isdigit() and int(limit) > 0

    judge("gh", ["gh", "api", "rate_limit", "--jq", ".resources.core.limit"], gh_answers)

    try:
        head = call(["git", "rev-parse", "HEAD"], sandboxed=False)
    except _ERRORS:
        head = None
    if head is not None and head.returncode == 0:
        # A transaction that takes the ref lock a commit takes, then aborts:
        # no ref is ever created, and an interrupted write reads EOF and aborts.
        txn = f"start\ncreate refs/fno-probe/{nonce} {head.stdout.strip()}\nprepare\nabort\n"
        judge(
            "git",
            ["git", "update-ref", "--stdin"],
            lambda proc: "prepare: ok" in proc.stdout.splitlines(),
            stdin=txn,
        )

    if blocked:
        return SandboxProbe("blocked", blocked, "; ".join(unjudged))
    if unjudged:
        return SandboxProbe("unknown", note="; ".join(unjudged))
    return SandboxProbe("reachable")
