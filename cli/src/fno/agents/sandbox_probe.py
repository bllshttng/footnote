"""Probe a bounded codex worker's sandbox before it is dispatched.

A codex worker in the ``workspace-write`` sandbox can launch, report ready,
and then wait forever on a tool the sandbox blocks: ``gh`` with no network, or
a git ref write with no writable git dir. ``codex sandbox`` runs a command
under the same seatbelt policy and ``config.toml`` a worker gets, so each tool
is checked from inside it against a marker only that tool's success produces.

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
from typing import Callable, Literal

EXIT_SANDBOX_UNREACHABLE = 82

_TIMEOUT_SECS = 15
_ERRORS = (OSError, subprocess.SubprocessError)


@dataclass(frozen=True)
class SandboxProbe:
    verdict: Literal["reachable", "blocked", "unknown"]
    blocked: list[tuple[str, str]] = field(default_factory=list)
    note: str = ""


def _first_line(text: str) -> str:
    # gh states the cause first and a status-page pointer after it.
    lines = [line.strip() for line in (text or "").splitlines() if line.strip()]
    return lines[0][:160] if lines else ""


def _why(proc: "subprocess.CompletedProcess[str]") -> str:
    return _first_line(proc.stderr) or f"exit {proc.returncode}"


def probe_codex_sandbox(
    cwd: Path,
    *,
    run: Callable[..., "subprocess.CompletedProcess[str]"] = subprocess.run,
) -> SandboxProbe:
    """Check that ``gh`` and a git ref write work inside the worker's sandbox."""
    from fno.agents.harnesses.codex import git_writable_config_args

    # The same roots override the resume lane grants, so the probe runs under
    # the grant the worker gets rather than a narrower default.
    sandbox = [
        "codex", "sandbox", "-c", 'sandbox_mode="workspace-write"',
        *git_writable_config_args(cwd), "--",
    ]

    def call(argv: list[str], *, sandboxed: bool = True) -> "subprocess.CompletedProcess[str]":
        return run(
            [*sandbox, *argv] if sandboxed else argv,
            cwd=str(cwd),
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

    gh_argv = ["gh", "api", "rate_limit", "--jq", ".resources.core.limit"]

    def gh_answers(proc: "subprocess.CompletedProcess[str]") -> bool:
        limit = proc.stdout.strip()
        return proc.returncode == 0 and limit.isdigit() and int(limit) > 0

    try:
        inside = call(gh_argv)
        if not gh_answers(inside):
            outside = call(gh_argv, sandboxed=False)
            if gh_answers(outside):
                blocked.append(("gh", _why(inside)))
            else:
                unjudged.append(f"gh fails outside the sandbox too ({_why(outside)})")
    except _ERRORS as exc:
        unjudged.append(f"gh: {type(exc).__name__}: {exc}"[:160])

    try:
        head = call(["git", "rev-parse", "HEAD"], sandboxed=False)
    except _ERRORS:
        head = None
    if head is not None and head.returncode == 0:
        ref = f"refs/fno-probe/{nonce}"
        sha = head.stdout.strip()

        def ref_landed() -> bool:
            back = call(["git", "rev-parse", "--verify", "-q", ref], sandboxed=False)
            return back.stdout.strip() == sha

        try:
            write = call(["git", "update-ref", ref, "HEAD"])
            if not ref_landed():
                outside = call(["git", "update-ref", ref, "HEAD"], sandboxed=False)
                if ref_landed():
                    blocked.append(("git", _why(write)))
                else:
                    unjudged.append(f"git ref write fails outside the sandbox too ({_why(outside)})")
        except _ERRORS as exc:
            unjudged.append(f"git: {type(exc).__name__}: {exc}"[:160])
        finally:
            try:
                call(["git", "update-ref", "-d", ref], sandboxed=False)
            except _ERRORS:
                pass

    if blocked:
        return SandboxProbe("blocked", blocked, "; ".join(unjudged))
    if unjudged:
        return SandboxProbe("unknown", note="; ".join(unjudged))
    return SandboxProbe("reachable")
