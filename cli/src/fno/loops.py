"""Loop levels + global pause-all kill switch.

Substrate only: no standing loop ships in this module. Every later loop is
born with a pause button by reading ``loops_paused()`` at tick start (paused
= log one line, exit 0) and its configured autonomy via ``loop_level(name)``.
No daemon, no process registry - loops are cron/Actions-triggered CLI ticks;
the sentinel file is the only coordination point.
"""
from __future__ import annotations

import json
import logging
import subprocess
import sys
from typing import Optional

import typer

from fno import paths
from fno.config import LoopEntry, load_settings

_LOG = logging.getLogger(__name__)

loops_app = typer.Typer(
    name="loops", no_args_is_help=True, help="Loop level config + pause-all kill switch."
)


@loops_app.callback()
def _loops_callback() -> None:
    """No-op: keeps Typer from collapsing multi-command sub-apps into one."""


def loop_level(name: str) -> str:
    """Return the configured level for loop *name*.

    Never raises: an unconfigured or unrecognized name always falls back to
    "report" (observe only), the safest default for a loop that hasn't
    graduated yet.
    """
    entry = load_settings().loops.get(name)
    return entry.level if entry is not None else LoopEntry().level


def _rust_loops_call(action: str, args: list[str] | None = None) -> dict:
    """Call the Rust owner of the global pause sentinel."""
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    binary_name = str(binary) if binary is not None else "<missing>"
    if binary is None:
        raise RuntimeError(f"fno-agents binary {binary_name} is unavailable")
    argv = [str(binary), "loops", action, *(args or []), "--json"]
    try:
        proc = subprocess.run(
            argv, capture_output=True, text=True, check=False, timeout=5
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise RuntimeError(f"fno-agents binary {binary_name} failed: {exc}") from exc
    if proc.returncode != 0:
        raise RuntimeError(
            f"fno-agents binary {binary_name} exited {proc.returncode}: "
            f"{proc.stderr.strip()[:200]}"
        )
    try:
        payload = json.loads(proc.stdout)
    except ValueError as exc:
        raise RuntimeError(f"fno-agents binary {binary_name} returned bad JSON: {exc}") from exc
    if not isinstance(payload, dict):
        raise RuntimeError(f"fno-agents binary {binary_name} returned non-object JSON")
    return payload


def loops_paused() -> bool:
    """Return the Rust owner's pause verdict, failing closed on any failure."""
    try:
        payload = _rust_loops_call("paused")
    except Exception as exc:  # noqa: BLE001 - safety switch must fail closed
        _LOG.warning("loops-paused check failed closed: %s", exc)
        return True
    return payload.get("paused") is not False


def refuse_if_paused(*, json_out: bool) -> None:
    """Stop dispatch while paused, leaving explain/read paths available."""
    if not loops_paused():
        return
    if json_out:
        typer.echo(json.dumps({"skipped": "loops_paused"}))
    else:
        typer.echo("advance: loops paused (fno do loops resume-all to lift)")
    raise typer.Exit(code=0)


def _last_tick(name: str) -> Optional[str]:
    """Best-effort: the most recent ``loop_tick`` event timestamp for *name*.

    Returns None when no such event exists - expected for every configured
    loop today, since this node ships the substrate before any loop turns on.
    A read/parse failure degrades to None rather than raising (ls is a
    read-only status view, never a gate).
    """
    from fno.events.log import read_events

    events_path = paths.project_log("events.jsonl")
    try:
        events = read_events(events_path)
    except (OSError, ValueError) as exc:
        _LOG.warning("could not read %s for loop tick lookup: %s", events_path, exc)
        return None
    last: Optional[str] = None
    for event in events:
        if event.get("type") != "loop_tick":
            continue
        if (event.get("data") or {}).get("name") != name:
            continue
        ts = event.get("ts")
        if ts and (last is None or ts > last):
            last = ts
    return last


def _run_loops_passthrough(action: str, args: list[str]) -> int:
    """Shell straight through to the Rust owner, argv and exit code unchanged.

    The TTL/reason parsing and the mail leg both live in
    ``crates/fno-agents/src/loops_pause.rs`` now; this wrapper adds nothing
    and drops nothing, so its own flags never drift from the Rust ones.

    Captures rather than inherits stdio: Click's test runner (and any other
    caller that redirects ``sys.stdout``) only sees output written through
    Python's stream objects, not a child's raw inherited file descriptor.
    """
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo("fno-agents binary is unavailable", err=True)
        return 1
    proc = subprocess.run(
        [str(binary), "loops", action, *args], capture_output=True, text=True, check=False
    )
    if proc.stdout:
        sys.stdout.write(proc.stdout)
    if proc.stderr:
        sys.stderr.write(proc.stderr)
    return proc.returncode


@loops_app.command(
    "pause-all",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def cmd_pause_all(ctx: typer.Context) -> None:
    """Pause every loop and hold this session's mail for the same window.

    Pass-through to the Rust owner: `--who <w> [--ttl <dur>] [--reason <text>]`.
    """
    raise typer.Exit(code=_run_loops_passthrough("pause-all", ctx.args))


@loops_app.command(
    "resume-all",
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
)
def cmd_resume_all(ctx: typer.Context) -> None:
    """Remove the pause-all sentinel and lift the held mail."""
    raise typer.Exit(code=_run_loops_passthrough("resume-all", ctx.args))


@loops_app.command("status")
def cmd_status() -> None:
    """Show the current pause-all sentinel, including an expired one."""
    state = _rust_loops_call("status")
    if state.get("state") == "corrupt":
        typer.echo(
            f"sentinel at {state.get('path', '<unknown>')} is corrupted; "
            "failing closed (treated as paused) - investigate"
        )
        return
    if state.get("state") == "clear":
        typer.echo("not paused")
        return
    if state.get("state") == "expired":
        typer.echo(f"expired (was paused by {state['who']} at {state['paused_at']})")
        return
    expiry = f", expires {state['expires_at']}" if state.get("expires_at") else ""
    typer.echo(f"paused by {state.get('who', 'unknown')} since {state.get('paused_at', 'unknown')}{expiry}")


@loops_app.command("ls")
def cmd_ls() -> None:
    """List configured loops, their level, and last-tick timestamp."""
    settings = load_settings()
    names = sorted(settings.loops)
    if not names:
        typer.echo("no loops configured")
        return
    for name in names:
        level = loop_level(name)
        last_tick = _last_tick(name) or "never"
        typer.echo(f"{name}\t{level}\t{last_tick}")
