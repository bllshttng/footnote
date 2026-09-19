"""fno do pr sync-canonical - transport over the native sync verb.

The sync, its catch-up sweep, and the staleness alarm are native:
crates/fno-agents/src/sync_canonical.rs owns the guard chain, the marker
write, the lease, the fnmatch path gate, the file-backed shell runner, and
every receipt string. This module only carries the JSON payload and echoes
the answer lines; a failure detail is computed natively and rides back, so a
reported cause is the real cause.
"""
from __future__ import annotations

import os
from typing import Any

import typer

# The sync runs a 600s shell plus gh probes; the door must outlast the
# verb's worst case with headroom, not report it unreachable. The read-only
# actions are bounded by their gh probes plus a fetch.
_TIMEOUT_SYNC_S = 900.0
_TIMEOUT_PROBE_S = 120.0


def _answer_fields(answer: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in answer.items() if k not in ("exit", "stdout", "stderr")}


def _echo_lines(answer: dict[str, Any]) -> None:
    for line in answer.get("stdout") or []:
        typer.echo(line)
    for line in answer.get("stderr") or []:
        typer.echo(line, err=True)


def run_sync_canonical(pr_number: int) -> int:
    """Run the canonical sync for a merged PR. Returns the process exit code."""
    from fno.rust_binary import VerbUnavailable, verb_call

    try:
        answer = verb_call(
            "sync-canonical",
            {"action": "sync", "cwd": os.getcwd(), "pr": pr_number},
            timeout=_TIMEOUT_SYNC_S,
        )
    except VerbUnavailable as exc:
        # The marker stays withheld, so the next reconcile retries.
        typer.echo(
            f"post-merge sync: native verb unavailable ({exc}); skipping", err=True
        )
        return 1
    _echo_lines(answer)
    return int(answer.get("exit", 1))


def run_sync_catchup(*, echo: bool = True) -> dict[str, Any]:
    """Catch-up sweep over the native verb; ``echo=False`` suppresses printed lines for a ``--json`` caller."""
    from fno.rust_binary import VerbUnavailable, verb_call

    try:
        answer = verb_call(
            "sync-canonical",
            {"action": "catchup", "cwd": os.getcwd()},
            timeout=_TIMEOUT_PROBE_S,
        )
    except VerbUnavailable as exc:
        return {
            "outcome": "unknown",
            "detail": f"native verb unavailable: {exc}",
        }
    if echo:
        _echo_lines(answer)
    return _answer_fields(answer)


def sync_staleness(*, fetch: bool = False) -> dict[str, Any]:
    """Is the canonical checkout current with recently-merged PRs?

    Read-only, so it is safe for ``fno doctor`` to call regardless of
    ``post_merge.auto_run`` - reporting is not acting. ``fetch`` rides the
    payload so the divergence read refreshes the remote-tracking ref first; a
    human-facing caller wants that, the 5-minute tick does not.
    """
    from fno.rust_binary import VerbUnavailable, verb_call

    try:
        answer = verb_call(
            "sync-canonical",
            {"action": "staleness", "cwd": os.getcwd(), "fetch": fetch},
            timeout=_TIMEOUT_PROBE_S,
        )
    except VerbUnavailable as exc:
        return {
            "state": "unknown",
            "markerless": [],
            "behind": None,
            "detail": f"native verb unavailable: {exc}",
        }
    return _answer_fields(answer)
