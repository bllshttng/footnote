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
from typing import Optional

import typer

from fno import paths
from fno.config import load_settings

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
    return entry.level if entry is not None else "report"


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


def pause_all(*, who: str, ttl_ms: Optional[int] = None) -> dict:
    args = ["--who", who]
    if ttl_ms is not None:
        args += ["--ttl-ms", str(ttl_ms)]
    return _rust_loops_call("pause-all", args)


def resume_all() -> bool:
    return bool(_rust_loops_call("resume-all").get("resumed"))


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


_TTL_PATTERN_HELP = "duration like '30m', '2h', '1d' (default: no expiry)"


def _parse_ttl_ms(value: Optional[str]) -> Optional[int]:
    if not value:
        return None
    import re

    m = re.match(r"^\s*(\d+)\s*([smhd])\s*$", value, re.IGNORECASE)
    if not m:
        raise typer.BadParameter(f"invalid TTL format: {value!r} ({_TTL_PATTERN_HELP})")
    n = int(m.group(1))
    if n == 0:
        # A zero TTL is falsy in Python, so `pause_all` would read it as "no
        # expiry" (ttl_ms=0 -> None) instead of "expires immediately" -
        # reject it outright rather than silently pausing forever.
        raise typer.BadParameter(f"TTL must be > 0: {value!r}")
    unit = m.group(2).lower()
    seconds = {"s": 1, "m": 60, "h": 3600, "d": 86400}[unit]
    return n * seconds * 1000


@loops_app.command("pause-all")
def cmd_pause_all(
    ttl: Optional[str] = typer.Option(None, "--ttl", help=_TTL_PATTERN_HELP),
    who: str = typer.Option("operator", "--who", help="Who is pausing (for status display)."),
) -> None:
    """Pause every loop: each tick sees loops_paused()==True and exits 0."""
    state = pause_all(who=who, ttl_ms=_parse_ttl_ms(ttl))
    expiry = f", expires {state['expires_at']}" if state.get("expires_at") else ""
    typer.echo(f"paused by {state.get('who', who)}{expiry}")


@loops_app.command("resume-all")
def cmd_resume_all() -> None:
    """Remove the pause-all sentinel."""
    was_paused = resume_all()
    typer.echo("resumed" if was_paused else "was not paused")


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
