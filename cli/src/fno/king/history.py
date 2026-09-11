"""``fno agents king history`` - scope resolution and native-read relay.

The scan itself is the native ``king-history`` verb
(crates/fno-agents/src/king_history.rs); contract: docs/architecture/reign.md.
"""
from __future__ import annotations

import subprocess
from pathlib import Path


class HistoryUnreadable(Exception):
    """No resolvable crown, or a corrupt journal line."""


def canonicalize_scope(scope: str) -> str:
    """The stored form of a crown scope, through the same path king init uses."""
    from fno.agents.crown import _canonical_members, canonical_scope

    return canonical_scope(list(_canonical_members(scope)))


def resolve_scope(explicit: str) -> str:
    """Explicit ``--scope`` wins; else the caller's crown, positively resolved."""
    if explicit.strip():
        canonical = canonicalize_scope(explicit)
        if not canonical:
            raise HistoryUnreadable("--scope names no crown territory.")
        return canonical

    from fno.agents.crown import AGENT_UNREGISTERED, REGISTRY_UNREADABLE, calling_agent_row

    try:
        row = calling_agent_row()
    except Exception as exc:  # noqa: BLE001 - refuse, never crash
        raise HistoryUnreadable(f"cannot resolve the caller's crown: {exc}") from exc
    if row is REGISTRY_UNREADABLE or row is AGENT_UNREGISTERED:
        raise HistoryUnreadable(
            "cannot resolve the caller's crown: no crowned registry row. "
            "Pass --scope explicitly."
        )
    own = getattr(row, "crown_scope", None)
    if not own:
        raise HistoryUnreadable(
            "this session holds no crown. Pass --scope <territory>."
        )
    return canonicalize_scope(own)


def run_native(events_path: Path, scope: str, as_json: bool) -> tuple[int, str, str]:
    """Relay to the native ``king-history`` read; ``(code, stdout, stderr)``."""
    from fno.agents.rust_runtime import refuse_without_binary
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        refuse_without_binary("king history")
    argv = [str(binary), "king-history", "--scope", scope, "--events-path", str(events_path)]
    if as_json:
        argv.append("--json")
    proc = subprocess.run(argv, capture_output=True, text=True, check=False)
    return proc.returncode, proc.stdout, proc.stderr
