"""Probe a codex worker's sandbox before launch (docs/architecture/coordination.md)."""
from __future__ import annotations

import secrets
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Literal

EXIT_SANDBOX_UNREACHABLE = 82


@dataclass(frozen=True)
class SandboxProbe:
    verdict: Literal["reachable", "blocked", "unknown"]
    blocked: list[tuple[str, str]] = field(default_factory=list)
    note: str = ""


def _why(proc: Any) -> str:
    # gh states its cause at the start of its first line, git's lock error at the end.
    line = next((ln.strip() for ln in (proc.stderr or "").splitlines() if ln.strip()), "")
    line = line if len(line) <= 160 else f"{line[:60]}...{line[-97:]}"
    return line or f"exit {proc.returncode}"


def probe_codex_sandbox(cwd: Path, *, run: Callable[..., Any] = subprocess.run) -> SandboxProbe:
    """Check that ``gh`` and a git ref lock work inside the worker's codex sandbox."""
    from fno.agents.harnesses.codex import git_writable_config_args as grant

    # The worker's own grant, so the probe is never narrower than the worker.
    sandbox = ["codex", "sandbox", "-c", 'sandbox_mode="workspace-write"', *grant(cwd), "--"]

    def call(argv: list[str], inside: bool = True, stdin: str | None = None) -> Any:
        wrapped = [*sandbox, *argv] if inside else argv
        try:
            return run(wrapped, cwd=str(cwd), input=stdin, capture_output=True, text=True, timeout=15)
        except (OSError, subprocess.SubprocessError) as exc:
            return subprocess.CompletedProcess(wrapped, -1, "", f"{type(exc).__name__}: {exc}")

    nonce = secrets.token_hex(8)
    control = call(["/bin/echo", nonce])
    if control.returncode != 0 or control.stdout.strip() != nonce:
        return SandboxProbe("unknown", note=_why(control))
    blocked: list[tuple[str, str]] = []
    unjudged: list[str] = []

    def judge(tool: str, argv: list[str], ok: Callable[..., bool], txn: str | None = None) -> None:
        inside = call(argv, stdin=txn)
        if ok(inside):
            return
        outside = call(argv, False, txn)
        if ok(outside):  # Only a tool that answers outside is blocked BY the sandbox.
            blocked.append((tool, _why(inside)))
        else:
            unjudged.append(f"{tool} fails outside the sandbox too ({_why(outside)})")

    judge("gh", ["gh", "api", "rate_limit", "--jq", ".resources.core.limit"],
          lambda p: p.returncode == 0 and p.stdout.strip().isdigit() and int(p.stdout) > 0)
    head = call(["git", "rev-parse", "HEAD"], False)
    if head.returncode == 0:
        # Take the lock a commit takes, then abort: no ref is created, and EOF aborts too.
        txn = f"start\ncreate refs/fno-probe/{nonce} {head.stdout.strip()}\nprepare\nabort\n"
        judge("git", ["git", "update-ref", "--stdin"],
              lambda p: "prepare: ok" in p.stdout.splitlines(), txn)
    if blocked:
        return SandboxProbe("blocked", blocked, "; ".join(unjudged))
    if unjudged:
        return SandboxProbe("unknown", note="; ".join(unjudged))
    return SandboxProbe("reachable")
