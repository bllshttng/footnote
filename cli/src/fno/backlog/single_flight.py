"""One in flight per backlog arm scope (advance, reconcile).

The lock is native (`fno-agents claim flight-acquire` / `flight-release`); this
module is the shim the two backlog verbs call. A second invocation for an
in-flight scope reports `held` and exits 0: a skipped tick is correct behavior
when the previous tick is still running.
"""

from __future__ import annotations

import contextlib
import json
import os
import subprocess
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterator, Optional

import typer

from fno.claims.io import claims_root_for
from fno.rust_binary import resolve_binary

# Twelve-minute reconcile runs are measured; 30 minutes bounds a lost holder.
FLIGHT_TTL_MS = 30 * 60 * 1000


def advance_flight_key(epic: Optional[str]) -> str:
    """An epic converge and a board advance are different work; same-scope
    copies are the stacking this deletes."""
    return f"flight:backlog-advance:epic:{epic}" if epic else "flight:backlog-advance"


def reconcile_flight_key(*, node: Optional[str], pr_number: Optional[int], repo: Optional[str] = None) -> str:
    """A full sweep owns the graph; a --pr-number pass owns ONE repo's PR (two
    repos can carry the same number); a --node pass owns one node id, which is
    globally unique."""
    if node:
        return f"flight:backlog-reconcile:node:{node}"
    if pr_number is not None:
        return f"flight:backlog-reconcile:pr:{repo or 'unresolved'}:{pr_number}"
    return "flight:backlog-reconcile"


@dataclass
class Flight:
    """The gate result: this invocation holds the scope (`held` False, call
    release() after the work) or stood down for the in-flight holder."""

    key: str
    holder: str
    held: bool = False
    held_for_s: int = 0
    requests: int = 0

    def release(self) -> None:
        try:
            _verb([
                "claim", "flight-release", self.key,
                "--holder", self.holder,
                "--claims-root", str(claims_root_for(self.key)),
            ])
        except Exception:
            pass  # the TTL plus the pid probe retire it; never mask the work's outcome

    def payload(self) -> dict[str, object]:
        return {"held": True, "requests": self.requests, "holder": self.holder, "held_for_s": self.held_for_s}


def acquire_flight(key: str, *, scope: str) -> Optional[Flight]:
    """Take the gate, or report it held. None (fail-open) when the lock itself
    is unavailable - no fno-agents binary, one older than the verb, spawn
    trouble, a gate-side error - and the caller proceeds ungated, the
    pre-gate behavior. The holder is unique per invocation and the claim
    carries our pid, so the verb probes the pid and reclaims a dead holder
    itself: the holder here is a one-shot subprocess, so the
    transcript-liveness basis the claims layer prefers for node claims is the
    wrong policy."""
    if resolve_binary() is None:
        typer.echo(
            f"warning: single-flight gate unavailable for {key} (no fno-agents binary); proceeding ungated",
            err=True,
        )
        return None
    try:
        receipt = _verb([
            "claim", "flight-acquire", key,
            "--scope", scope,
            "--ttl-ms", str(FLIGHT_TTL_MS),
            "--holder", f"single-flight:{os.getpid()}:{uuid.uuid4().hex[:8]}",
            "--pid", str(os.getpid()),
            "--claims-root", str(claims_root_for(key)),
        ])
    except Exception as exc:  # noqa: BLE001 - fail open, never break the verb
        typer.echo(f"warning: single-flight gate unavailable for {key} ({exc}); proceeding ungated", err=True)
        return None
    if receipt is None:
        return None
    return Flight(
        key=key,
        holder=str(receipt.get("holder") or "unknown"),
        held=not receipt.get("acquired"),
        held_for_s=int(receipt.get("held_for_s") or 0),
        requests=int(receipt.get("requests") or 0),
    )


def _verb(argv: list[str]) -> Optional[dict]:
    """Run one `fno-agents claim` lock operation and parse its JSON receipt;
    None on any failure, which the caller treats as fail-open."""
    binary = resolve_binary()
    if binary is None:
        return None
    try:
        proc = subprocess.run([str(binary), *argv], capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    try:
        return json.loads(proc.stdout or "{}")
    except json.JSONDecodeError:
        return None


def _report_held(flight: Flight, verb: str, *, json_out: bool, extra: Optional[dict] = None) -> None:
    """Print the held receipt. Exit stays 0: a held tick is not an error."""
    if json_out:
        payload = flight.payload()
        if extra:
            payload.update(extra)
        typer.echo(json.dumps(payload))
        return
    typer.echo(
        f"{verb}: held, a run for this scope is already in flight "
        f"(holder={flight.holder}, held_for_s={flight.held_for_s}s, requests={flight.requests}); "
        "the in-flight run owns the work, this one stood down"
    )


@contextlib.contextmanager
def advance_flight_scope(epic: Optional[str], *, json_out: bool) -> Iterator[bool]:
    """Gate one advance invocation; yields False (held, already reported) and
    the caller returns. --stop never enters: a control action is not a
    converge and never queues behind its own drain. The board held receipt
    carries decision=held, so the documented --json shape survives."""
    extra = None if epic else {"decision": "held"}
    scope = "advance --epic" if epic else "advance"
    with _flight_scope(advance_flight_key(epic), scope, "backlog advance", json_out, extra) as ok:
        yield ok


def reconcile_gate(*, dry_run: bool, node: Optional[str], json_out: bool, pr_number: Optional[int],
                   repo: Optional[str] = None, once: Callable[[], None]) -> None:
    """cmd_reconcile's entry: the mutual-exclusion refusal (a bad invocation
    is refused even while the scope is held), the dry-run bypass (--dry-run
    mutates nothing and stays readable mid-sweep), then the gate."""
    if node is not None and pr_number is not None:
        raise typer.BadParameter(
            "--node and --pr-number are mutually exclusive: --pr-number "
            "already scopes the scan to every node its own trailer claims, "
            "which --node cannot narrow without silently stranding the "
            "other claimed nodes stamped-but-unclosed. Run them separately."
        )
    if dry_run:
        once()
        return
    if pr_number is not None and repo is None:
        from fno.graph._reconcile import resolve_current_repo_slug

        repo = resolve_current_repo_slug(str(Path.cwd())) or "unresolved"
    with _flight_scope(reconcile_flight_key(node=node, pr_number=pr_number, repo=repo), "reconcile",
                       "backlog reconcile", json_out, None) as ok:
        if ok:
            once()


@contextlib.contextmanager
def _flight_scope(key: str, scope: str, verb: str, json_out: bool,
                  extra: Optional[dict]) -> Iterator[bool]:
    flight = acquire_flight(key, scope=scope)
    try:
        if flight is not None and flight.held:
            _report_held(flight, verb, json_out=json_out, extra=extra)
            yield False
        else:
            yield True
    finally:
        if flight is not None and not flight.held:
            flight.release()
