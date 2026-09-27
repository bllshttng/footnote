"""Probe a codex worker's sandbox before launch (docs/architecture/coordination.md).

The probe lives in Rust (crates/fno-agents/src/sandbox_probe.rs); this is the
transport bridge. The verdict describes the worker's OWN requested posture
(never a hardcoded workspace-write), and a canary write inside the grant plus
its negative control are judged there. Exit 85 and the ``sandbox-probe:``
marker stay here: the spawn gate owns the refusal."""
from __future__ import annotations

import subprocess  # noqa: F401  (kept for the bridge's own subprocess use)
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

# Allocated by the table in fno/agents/spawn_gate.py; 82 was the Python gate's
# EXIT_FLEET_STOP and a number that means two things cannot be read.
EXIT_SANDBOX_UNREACHABLE = 85


@dataclass(frozen=True)
class SandboxProbe:
    verdict: Any  # "reachable" | "blocked" | "unknown"
    blocked: list[tuple[str, str]] = field(default_factory=list)
    note: str = ""


def probe_codex_sandbox(cwd: Path, *, mode: str | None = None) -> SandboxProbe:
    """Judge the sandbox the worker is about to run under, via the Rust probe.

    An unavailable owner answers ``unknown`` with the reason, never a guessed
    ``reachable``: an unjudged spawn launches with a named caveat, a blocked
    one refuses at exit 85."""
    from fno.rust_binary import VerbUnavailable, verb_call

    try:
        answer = verb_call(
            "sandbox-probe", {"cwd": str(cwd), "mode": mode}, VerbUnavailable
        )
    except VerbUnavailable as exc:
        return SandboxProbe("unknown", note=f"sandbox probe owner unavailable: {exc}")
    blocked = [(str(b[0]), str(b[1])) for b in answer.get("blocked") or [] if isinstance(b, (list, tuple)) and len(b) >= 2]
    note = str(answer.get("note") or "")
    return SandboxProbe(
        str(answer.get("verdict") or "unknown"),
        blocked,
        note,
    )
