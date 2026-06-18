"""Live TUI for ``fno megawalk watch``.

Repointed in task 2.4 (ab-7303e5d7) from megawalk-state.md /
megawalk-events.jsonl to canonical .fno/events.jsonl (source "loop").

Renders loop-stream events at ~1Hz:
  - loop_unit_dispatched: unit started
  - node_closed:          unit finished (close="closed"|"parked"|"refused")
  - walk_paused:          walk stopped by policy (consecutive_failures | p0_failed)
  - node_failed:          unit runtime error
  - loop_terminated:      walk finished (reason: NoWork | Budget | NoProgress | Interrupted)

Header derives from the journal (latest walk state) instead of megawalk-state.md.
In-flight = units with loop_unit_dispatched but no node_closed yet.
Sequential walk: at most 1 in-flight at a time.

Silent-failure guard: an empty or unreadable journal renders an explicit
error panel, never a blank screen (Locked Decision 11 requirement).

Use Ctrl-C to exit cleanly.
"""
from __future__ import annotations

import json
import sys
import time
from collections import deque
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional, Union

from rich.console import Console, RenderableType
from rich.layout import Layout
from rich.live import Live
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

REFRESH_HZ = 1.0
EVENT_TAIL = 20
ELAPSED_UNKNOWN = "?"

# Loop event kinds emitted by fno-agents (source "loop")
_DISPATCH_KIND = "loop_unit_dispatched"
_CLOSED_KIND = "node_closed"
_PAUSED_KIND = "walk_paused"
_FAILED_KIND = "node_failed"
_TERMINATED_KIND = "loop_terminated"
_TERMINATION_KIND = "termination"  # session-level termination event


# ---------------------------------------------------------------------------
# Time helpers (identical to the prior module; keep the monkey-patch seam)
# ---------------------------------------------------------------------------


def _now_iso() -> str:
    """Current time as ISO-8601 UTC. Monkey-patched in tests for determinism."""
    return datetime.now(timezone.utc).isoformat()


def _parse_iso(s: Optional[str]) -> Optional[datetime]:
    """Parse an ISO timestamp tolerantly. ``None`` if parsing fails."""
    if not s:
        return None
    try:
        return datetime.fromisoformat(s.replace("Z", "+00:00"))
    except (ValueError, TypeError):
        return None


def _format_elapsed(seconds: Union[int, float]) -> str:
    """Render seconds as ``42s`` / ``2m 5s`` / ``1h 2m``."""
    if seconds <= 0:
        return "0s"
    s = int(seconds)
    if s < 60:
        return f"{s}s"
    if s < 3600:
        return f"{s // 60}m {s % 60}s"
    return f"{s // 3600}h {(s % 3600) // 60}m"


def _format_elapsed_or_unknown(iso_ts: Optional[str]) -> str:
    """Return formatted elapsed time, or ``?`` when ``iso_ts`` is unparseable."""
    if not iso_ts:
        return ELAPSED_UNKNOWN
    started = _parse_iso(iso_ts)
    if started is None:
        return ELAPSED_UNKNOWN
    now = _parse_iso(_now_iso())
    if now is None:
        return ELAPSED_UNKNOWN
    delta = (now - started).total_seconds()
    return _format_elapsed(delta)


# ---------------------------------------------------------------------------
# I/O helpers
# ---------------------------------------------------------------------------


def _tail_jsonl(path: Path, n: int) -> list[dict]:
    """Return the last ``n`` parseable JSONL entries from ``path``.

    Tolerates truncated mid-write lines (silently dropped). Missing file
    returns an empty list rather than raising.

    File-permission errors propagate so the watch loop can render an
    error panel instead of pretending there are no events.

    The dropped-line count for the current call is exposed on
    ``_tail_jsonl.last_dropped`` so the events panel can surface it.
    """
    _tail_jsonl.last_dropped = 0  # type: ignore[attr-defined]
    if not path.exists():
        return []
    parsed: deque[dict] = deque(maxlen=n)
    dropped = 0
    try:
        with path.open("r", encoding="utf-8", errors="replace") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    parsed.append(json.loads(line))
                except json.JSONDecodeError:
                    dropped += 1
                    continue
    except FileNotFoundError:
        return []
    _tail_jsonl.last_dropped = dropped  # type: ignore[attr-defined]
    return list(parsed)


_tail_jsonl.last_dropped = 0  # type: ignore[attr-defined]


# ---------------------------------------------------------------------------
# Walk state derived from journal events
# ---------------------------------------------------------------------------


class WalkState:
    """Derived from scanning events.jsonl for loop-source events.

    Sequential walk: in_flight has at most one entry.
    """

    def __init__(self) -> None:
        self.status: str = "starting"
        self.started_at: Optional[str] = None
        self.pause_policy: Optional[str] = None
        self.pause_detail: Optional[str] = None
        self.terminated_reason: Optional[str] = None
        # unit_id -> dispatched_at timestamp (in-flight if no node_closed yet)
        self.in_flight: dict[str, str] = {}
        # list of (unit_id, close_outcome) for recently closed units
        self.recently_closed: list[tuple[str, str]] = []

    @classmethod
    def from_events(cls, events: list[dict]) -> "WalkState":
        """Derive walk state by replaying all loop-source events in order."""
        state = cls()
        for ev in events:
            source = ev.get("source", "")
            kind = ev.get("type") or ev.get("event", "")
            data = ev.get("data") or {}
            ts = ev.get("ts", "")

            # Accept both flat events (legacy) and canonical envelope.
            # Canonical envelope: {"ts":..., "type":..., "source":"loop", "data":{...}}
            if source not in ("loop", "") and kind not in (
                _DISPATCH_KIND, _CLOSED_KIND, _PAUSED_KIND,
                _FAILED_KIND, _TERMINATED_KIND, _TERMINATION_KIND,
            ):
                continue

            if kind == _DISPATCH_KIND:
                unit_id = data.get("unit_id") or ev.get("unit_id", "?")
                if state.started_at is None:
                    state.started_at = ts
                state.status = "running"
                state.pause_policy = None
                state.in_flight[unit_id] = ts

            elif kind == _CLOSED_KIND:
                unit_id = data.get("unit_id") or ev.get("unit_id", "?")
                close = data.get("close") or ev.get("close", "closed")
                state.in_flight.pop(unit_id, None)
                state.recently_closed.append((unit_id, close))
                if len(state.recently_closed) > EVENT_TAIL:
                    state.recently_closed = state.recently_closed[-EVENT_TAIL:]

            elif kind == _PAUSED_KIND:
                state.status = "paused"
                state.pause_policy = data.get("policy") or ev.get("policy", "")
                state.pause_detail = data.get("detail") or ev.get("detail", "")

            elif kind == _FAILED_KIND:
                unit_id = data.get("unit_id") or ev.get("unit_id", "?")
                state.in_flight.pop(unit_id, None)

            elif kind in (_TERMINATED_KIND, _TERMINATION_KIND):
                reason = data.get("reason") or ev.get("reason", "")
                state.status = "terminated"
                state.terminated_reason = str(reason)
                state.in_flight.clear()

        return state


# ---------------------------------------------------------------------------
# Render functions
# ---------------------------------------------------------------------------


def render_header(walk: WalkState) -> Panel:
    """Render the header panel showing walk status."""
    parts: list[str] = []
    status_color = {
        "running": "green",
        "paused": "red",
        "terminated": "dim",
        "starting": "yellow",
    }.get(walk.status, "white")
    parts.append(f"[{status_color}]{walk.status}[/{status_color}]")

    if walk.started_at:
        parts.append(f"runtime={_format_elapsed_or_unknown(walk.started_at)}")

    if walk.pause_policy:
        detail = f": {walk.pause_detail}" if walk.pause_detail else ""
        parts.append(f"[red]paused({walk.pause_policy}{detail})[/red]")

    if walk.terminated_reason:
        parts.append(f"reason={walk.terminated_reason}")

    return Panel(Text.from_markup(" | ".join(parts)), title="Walk")


def render_in_flight(walk: WalkState) -> Panel:
    """Render the in-flight units panel (at most 1 for sequential walk)."""
    table = Table(show_header=True, header_style="bold", expand=True)
    table.add_column("unit_id")
    table.add_column("elapsed", justify="right")
    for unit_id, dispatched_at in walk.in_flight.items():
        elapsed = _format_elapsed_or_unknown(dispatched_at)
        table.add_row(unit_id, elapsed)
    title = f"In-flight ({len(walk.in_flight)})"
    return Panel(table, title=title)


def render_recently_closed(walk: WalkState) -> Panel:
    """Render the recently closed units panel."""
    table = Table(show_header=True, header_style="bold", expand=True)
    table.add_column("unit_id")
    table.add_column("outcome")
    for uid, outcome in reversed(walk.recently_closed[-10:]):
        color = "green" if outcome == "closed" else "yellow"
        table.add_row(uid, f"[{color}]{outcome}[/{color}]")
    return Panel(table, title=f"Recently closed ({len(walk.recently_closed)} total)")


def render_events(events_path: Path) -> Panel:
    """Render the raw events tail panel."""
    try:
        lines = _tail_jsonl(events_path, EVENT_TAIL)
    except OSError as exc:
        return Panel(
            Text(f"Error reading events: {exc}", style="red"),
            title="Events (tail 20)",
        )
    dropped = getattr(_tail_jsonl, "last_dropped", 0)
    rendered = Text()
    for ev in lines:
        ts = str(ev.get("ts", ""))[:19].replace("T", " ")
        kind = ev.get("type") or ev.get("event", "?")
        source = ev.get("source", "")
        data = ev.get("data") or {}
        rest = " ".join(
            f"{k}={v}"
            for k, v in data.items()
            if k not in {"ts"}
        ) if data else " ".join(
            f"{k}={v}"
            for k, v in ev.items()
            if k not in {"ts", "event", "type", "source", "data"}
        )
        src_tag = f"[{source}] " if source else ""
        rendered.append(f"{ts} {src_tag}{kind} {rest}\n")
    if not lines:
        rendered.append("(no events yet)\n", style="dim")
    if dropped:
        rendered.append(
            f"({dropped} malformed lines skipped)\n",
            style="dim yellow",
        )
    return Panel(rendered, title="Events (tail 20)")


def build_layout(walk: WalkState, events_path: Path) -> Layout:
    """Compose the four panels into a Rich Layout ready for printing."""
    layout = Layout()
    layout.split_column(
        Layout(name="header", size=3),
        Layout(name="body"),
        Layout(name="events", size=12),
    )
    layout["body"].split_row(
        Layout(name="in_flight"),
        Layout(name="closed"),
    )
    layout["header"].update(render_header(walk))
    layout["in_flight"].update(render_in_flight(walk))
    layout["closed"].update(render_recently_closed(walk))
    layout["events"].update(render_events(events_path))
    return layout


# ---------------------------------------------------------------------------
# Frame helper - the testable seam carved out of the live loop
# ---------------------------------------------------------------------------


def _render_one_frame(events_path: Path) -> RenderableType:
    """Render exactly one frame from events_path.

    The watch() live loop calls this repeatedly. Extracting it lets tests
    cover missing-journal and error paths without driving a Live context.

    Silent-failure guard: if the journal is absent or unreadable, renders
    an explicit error panel rather than a blank screen.
    """
    if not events_path.exists():
        return Panel(
            Text(
                f"Waiting for {events_path} ...\n"
                "Start a walk with: fno-agents loop run --driver megawalk",
                style="dim",
            ),
            title="Walk",
        )
    try:
        events = _tail_jsonl(events_path, n=1000)
    except OSError as exc:
        return Panel(
            Text(f"Cannot read journal: {exc}", style="red"),
            title="Walk",
        )
    walk = WalkState.from_events(events)
    return build_layout(walk, events_path)


# ---------------------------------------------------------------------------
# Main watch loop
# ---------------------------------------------------------------------------


def watch(events_path: Path) -> int:
    """Live-render the walk state until Ctrl-C.

    Repointed from (state_path, events_path) to events_path only.
    State is derived from the journal (WalkState.from_events).

    Returns:
        0 on clean Ctrl-C exit; 1 on any unexpected failure.
    """
    console = Console()
    try:
        with Live(
            console=console,
            refresh_per_second=REFRESH_HZ,
            screen=True,
            transient=False,
            auto_refresh=False,
        ) as live:
            while True:
                live.update(
                    _render_one_frame(events_path),
                    refresh=True,
                )
                time.sleep(1.0 / REFRESH_HZ)
    except KeyboardInterrupt:
        return 0
    except Exception as exc:  # noqa: BLE001
        print(f"megawalk watch: TUI crashed: {exc}", file=sys.stderr)
        return 1
