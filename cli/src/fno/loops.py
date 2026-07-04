"""Loop levels + global pause-all kill switch (x-ce71).

Substrate only: no standing loop ships in this module. Every later loop is
born with a pause button by reading ``loops_paused()`` at tick start (paused
= log one line, exit 0) and its configured autonomy via ``loop_level(name)``.
No daemon, no process registry - loops are cron/Actions-triggered CLI ticks;
the sentinel file is the only coordination point.
"""
from __future__ import annotations

import json
import logging
import time
from typing import Optional

import typer
from pydantic import BaseModel, ConfigDict

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
    entry = load_settings().config.loops.get(name)
    return entry.level if entry is not None else "report"


class PauseState(BaseModel):
    """The ``loops-paused.json`` sentinel body."""

    model_config = ConfigDict(extra="ignore")

    who: str
    paused_at: int  # epoch ms
    expires_at: Optional[int] = None  # epoch ms; None = no TTL


def _now_ms() -> int:
    return int(time.time() * 1000)


def is_expired(state: PauseState, *, now: Optional[int] = None) -> bool:
    if state.expires_at is None:
        return False
    return (now if now is not None else _now_ms()) >= state.expires_at


class SentinelCorrupted(Exception):
    """Raised by :func:`read_pause_state` when the sentinel exists but is unparseable."""


def read_pause_state() -> Optional[PauseState]:
    """Read the sentinel from disk in one pass (no check-then-read race), ignoring TTL expiry.

    Returns None only when the sentinel is genuinely absent. Raises
    :class:`SentinelCorrupted` (logged) when it exists but can't be parsed -
    callers that must fail CLOSED on corruption (:func:`loops_paused`,
    ``status``) catch that and treat it as paused.
    """
    p = paths.loops_paused_json()
    try:
        raw = p.read_text(encoding="utf-8")
    except FileNotFoundError:
        return None
    except OSError as exc:
        _LOG.warning("loops-paused sentinel at %s could not be read: %s", p, exc)
        raise SentinelCorrupted(str(p)) from exc
    try:
        return PauseState.model_validate(json.loads(raw))
    except ValueError as exc:
        _LOG.warning("loops-paused sentinel at %s is unreadable: %s", p, exc)
        raise SentinelCorrupted(str(p)) from exc


def loops_paused() -> bool:
    """True if the pause-all sentinel is in effect.

    Every loop tick calls this first; paused = log one line and exit 0.
    Fails CLOSED: a present-but-corrupted sentinel counts as paused (assume
    the worst) rather than letting every loop silently resume - this
    primitive exists to be a safety rail, not a best-effort convenience.
    """
    try:
        state = read_pause_state()
    except SentinelCorrupted:
        return True
    return state is not None and not is_expired(state)


def pause_all(*, who: str, ttl_ms: Optional[int] = None) -> PauseState:
    """Write the pause-all sentinel, replacing any prior one."""
    state = PauseState(
        who=who, paused_at=_now_ms(), expires_at=(_now_ms() + ttl_ms) if ttl_ms else None
    )
    p = paths.loops_paused_json()
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_name(p.name + ".tmp")
    tmp.write_text(state.model_dump_json(), encoding="utf-8")
    tmp.replace(p)
    return state


def resume_all() -> bool:
    """Remove the pause-all sentinel. Returns True if one was present."""
    try:
        paths.loops_paused_json().unlink()
    except FileNotFoundError:
        return False
    return True


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
    expiry = f", expires {state.expires_at}" if state.expires_at else ""
    typer.echo(f"paused by {state.who}{expiry}")


@loops_app.command("resume-all")
def cmd_resume_all() -> None:
    """Remove the pause-all sentinel."""
    was_paused = resume_all()
    typer.echo("resumed" if was_paused else "was not paused")


@loops_app.command("status")
def cmd_status() -> None:
    """Show the current pause-all sentinel, including an expired one."""
    try:
        state = read_pause_state()
    except SentinelCorrupted as exc:
        typer.echo(f"sentinel at {exc} is corrupted; failing closed (treated as paused) - investigate")
        return
    if state is None:
        typer.echo("not paused")
        return
    if is_expired(state):
        typer.echo(f"expired (was paused by {state.who} at {state.paused_at})")
        return
    expiry = f", expires {state.expires_at}" if state.expires_at else ""
    typer.echo(f"paused by {state.who} since {state.paused_at}{expiry}")


@loops_app.command("ls")
def cmd_ls() -> None:
    """List configured loops, their level, and last-tick timestamp."""
    settings = load_settings()
    names = sorted(settings.config.loops)
    if not names:
        typer.echo("no loops configured")
        return
    for name in names:
        level = loop_level(name)
        last_tick = _last_tick(name) or "never"
        typer.echo(f"{name}\t{level}\t{last_tick}")
