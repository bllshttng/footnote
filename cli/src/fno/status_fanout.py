"""Status-sink fanout: the dumb dispatcher (x-2057).

Layer 2 of the status-breakpoints protocol. Workers emit x-dbaf protocol-family
events (``task_started`` / ``task_done`` / ``blocked`` / ``run_summary``) once to
``.fno/events.jsonl`` and never know sinks exist. This module sweeps that log on
a tick and routes each event to configured external sinks per a per-sink filter.

Correctness spine (see the plan's Locked Decisions):
  - **Timestamp cursor, per-sink** at ``.fno/status-sinks/<name>.cursor`` - a byte
    offset dies at the 8MB rotation; the RFC3339-Z ``ts`` string is rotation-proof.
  - **Rotation catch-up:** ``events.jsonl`` renames to ``events.jsonl.1`` (single
    generation). When a cursor predates the active file's first line the tick
    drains ``.1`` first, so a rotation between ticks is transparent.
  - **One shared pass** from ``min(cursors)``, evaluating each line against every
    sink in memory - not one file pass per sink.
  - **At-least-once:** a sink's cursor advances past an event only after that
    event's dispatch attempt completes; connect-class failures short-circuit the
    sink's remaining batch WITHOUT advancing, so it retries next tick.
  - **READ only:** the fanout never writes back into ``events.jsonl``; drops go to
    ``.fno/status-sinks/<name>.errors.jsonl``.
"""
from __future__ import annotations

import json
import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Optional

import typer

from fno import paths
from fno.config import StatusFanoutConfig, StatusSinkConfig

# Dispatch outcomes. A dispatch attempt classifies its result so the tick knows
# whether to advance the cursor past the event (DELIVERED / DROPPED) or hold it
# and retry next tick (SHORT_CIRCUIT - a connect-class failure).
DELIVERED = "delivered"
DROPPED = "dropped"
SHORT_CIRCUIT = "short_circuit"

# A dispatcher takes (sink, event) and returns (status, detail). `detail` is a
# human string for the errors log on DROPPED / SHORT_CIRCUIT (empty on DELIVERED).
Dispatcher = Callable[[StatusSinkConfig, dict[str, Any]], "tuple[str, str]"]


@dataclass
class SinkResult:
    name: str
    matched: int = 0
    dispatched: int = 0
    dropped: int = 0
    short_circuited: bool = False
    new_cursor: Optional[str] = None


@dataclass
class TickResult:
    sinks: list[SinkResult] = field(default_factory=list)
    skipped_lines: int = 0
    locked_out: bool = False  # another tick held the per-project lock; skipped


# ── event stream (rotation-aware, skip-and-count) ───────────────────────────


def _parse_line(line: str) -> Optional[dict[str, Any]]:
    line = line.strip()
    if not line:
        return None
    try:
        obj = json.loads(line)
    except (ValueError, TypeError):
        return None
    return obj if isinstance(obj, dict) and isinstance(obj.get("ts"), str) else None


def _read_events(path: Path, after_ts: Optional[str]) -> "tuple[list[dict[str, Any]], int]":
    """Return (events with ts > after_ts, malformed_line_count) from one file.

    Streams line-by-line; never loads the whole file as one string. A malformed
    or ts-less line is skipped and counted (digest.rs posture), never fatal.
    """
    events: list[dict[str, Any]] = []
    skipped = 0
    try:
        with path.open("r", encoding="utf-8") as fh:
            for raw in fh:
                ev = _parse_line(raw)
                if ev is None:
                    if raw.strip():
                        skipped += 1
                    continue
                if after_ts is None or ev["ts"] > after_ts:
                    events.append(ev)
    except FileNotFoundError:
        return [], 0
    return events, skipped


def _stream_since(active: Path, after_ts: Optional[str]) -> "tuple[list[dict[str, Any]], int]":
    """All events with ts > after_ts, draining the rotated ``.1`` first when the
    cursor predates the active file's first line. Rotated history is prepended so
    the returned list stays ts-ordered (both files are individually ordered and
    ``.1`` is strictly older)."""
    rotated = active.with_name(active.name + ".1")
    active_events, active_skipped = _read_events(active, after_ts)
    # Only touch .1 when the cursor is behind the active file's first retained
    # line - otherwise the rotated tail is already covered by the cursor.
    need_rotated = rotated.exists() and (
        after_ts is None
        or not active_events
        or after_ts < active_events[0]["ts"]
    )
    if not need_rotated:
        return active_events, active_skipped
    rotated_events, rotated_skipped = _read_events(rotated, after_ts)
    return rotated_events + active_events, active_skipped + rotated_skipped


def _eof_ts(active: Path) -> Optional[str]:
    """The max ts currently in the active log, or None if empty/absent. A fresh
    sink initializes its cursor here so no historical event is replayed."""
    last: Optional[str] = None
    try:
        with active.open("r", encoding="utf-8") as fh:
            for raw in fh:
                ev = _parse_line(raw)
                if ev is not None:
                    last = ev["ts"]
    except FileNotFoundError:
        return None
    return last


# ── cursor io (atomic) ──────────────────────────────────────────────────────


def _cursor_path(name: str, project_root: Optional[Path]) -> Path:
    return paths.status_sinks_dir(project_root) / f"{name}.cursor"


def _errors_path(name: str, project_root: Optional[Path]) -> Path:
    return paths.status_sinks_dir(project_root) / f"{name}.errors.jsonl"


def _read_cursor(name: str, project_root: Optional[Path]) -> Optional[str]:
    try:
        return _cursor_path(name, project_root).read_text(encoding="utf-8").strip() or None
    except FileNotFoundError:
        return None


def _write_cursor(name: str, ts: str, project_root: Optional[Path]) -> None:
    path = _cursor_path(name, project_root)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(ts, encoding="utf-8")
    os.replace(tmp, path)  # atomic: a concurrent read never sees a torn value


def _log_error(name: str, project_root: Optional[Path], record: dict[str, Any]) -> None:
    path = _errors_path(name, project_root)
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(record, separators=(",", ":")) + "\n")
    except OSError:
        pass  # an unwritable error log must never break the tick


# ── filter ──────────────────────────────────────────────────────────────────


def _matches(sink: StatusSinkConfig, event: dict[str, Any]) -> bool:
    """A sink matches an event when the event's ``type`` is one of the sink's
    ``events`` AND every ``match`` key equals the event's envelope field.

    Empty ``events`` matches nothing (a sink names the kinds it wants; this keeps
    the fanout off the non-status noise - claim/loop_check/etc. - in the log).
    """
    if event.get("type") not in sink.events:
        return False
    return all(str(event.get(k)) == str(v) for k, v in sink.match.items())


# ── the tick ────────────────────────────────────────────────────────────────


def run_tick(
    project_root: Path,
    sinks: list[StatusSinkConfig],
    fanout: Optional[StatusFanoutConfig] = None,
    *,
    dry_run: bool = False,
    dispatch_fn: Optional[Dispatcher] = None,
) -> TickResult:
    """One fanout pass. `dispatch_fn` defaults to the real adapter router; tests
    inject a recording fake to exercise cursor/rotation/isolation independently."""
    fanout = fanout or StatusFanoutConfig()
    dispatch = dispatch_fn or (lambda s, e: dispatch_event(s, e, fanout, project_root))
    enabled = [s for s in sinks if s.enabled]
    if not enabled:
        return TickResult()  # clean no-op: no cursor writes, no lock churn

    lock = _TickLock(project_root)
    if not lock.acquire():
        return TickResult(locked_out=True)
    try:
        return _run_locked(project_root, enabled, dry_run, dispatch)
    finally:
        lock.release()


def _run_locked(
    project_root: Path,
    sinks: list[StatusSinkConfig],
    dry_run: bool,
    dispatch: Dispatcher,
) -> TickResult:
    active = paths.project_log("events.jsonl", project_root=project_root)
    eof = _eof_ts(active)

    # Resolve each sink's starting cursor; a fresh sink (no file) initializes at
    # EOF so no history is replayed. In dry-run we do not persist that init.
    cursors: dict[str, Optional[str]] = {}
    fresh: dict[str, bool] = {}
    for s in sinks:
        cur = _read_cursor(s.name, project_root)
        fresh[s.name] = cur is None
        cursors[s.name] = cur if cur is not None else eof

    non_null = [c for c in cursors.values() if c is not None]
    min_cursor = min(non_null) if non_null else None
    events, skipped = _stream_since(active, min_cursor)

    state = {s.name: SinkResult(name=s.name, new_cursor=cursors[s.name]) for s in sinks}
    by_name = {s.name: s for s in sinks}

    for event in events:
        ets = event["ts"]
        for s in sinks:
            st = state[s.name]
            if st.short_circuited:
                continue
            cur = st.new_cursor
            if cur is not None and ets <= cur:
                continue
            if not _matches(s, event):
                continue
            st.matched += 1
            if dry_run:
                continue
            try:
                status, detail = dispatch(s, event)
            except Exception as exc:  # per-sink isolation: never abort the pass
                status, detail = DROPPED, f"adapter raised: {exc}"
            if status == DELIVERED:
                st.dispatched += 1
                st.new_cursor = ets
            elif status == DROPPED:
                st.dropped += 1
                st.new_cursor = ets
                _log_error(s.name, project_root, {
                    "sink": s.name, "event_ts": ets,
                    "type": event.get("type"), "reason": detail, "class": "dropped"})
            else:  # SHORT_CIRCUIT: hold the cursor, retry this + later events next tick
                st.short_circuited = True
                _log_error(s.name, project_root, {
                    "sink": s.name, "event_ts": ets,
                    "type": event.get("type"), "reason": detail, "class": "short_circuit"})

    # Persist advanced cursors (fresh sinks persist their EOF init even with zero
    # dispatch, so the next tick has a floor and never backfills).
    if not dry_run:
        for s in sinks:
            st = state[s.name]
            if st.new_cursor is not None and (st.dispatched or st.dropped or fresh[s.name]):
                _write_cursor(s.name, st.new_cursor, project_root)

    return TickResult(sinks=[state[s.name] for s in sinks], skipped_lines=skipped)


# ── per-project tick lock ───────────────────────────────────────────────────


class _TickLock:
    """Non-blocking flock over ``.fno/status-sinks/.tick.lock``. A hand-run tick
    racing the daemon's tick just skips (locked_out) rather than double-advancing
    cursors."""

    def __init__(self, project_root: Path) -> None:
        self._path = paths.status_sinks_dir(project_root) / ".tick.lock"
        self._fd: Optional[int] = None

    def acquire(self) -> bool:
        import fcntl

        self._path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(self._path, os.O_CREAT | os.O_RDWR, 0o644)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            os.close(fd)
            return False
        self._fd = fd
        return True

    def release(self) -> None:
        if self._fd is not None:
            import fcntl

            try:
                fcntl.flock(self._fd, fcntl.LOCK_UN)
            finally:
                os.close(self._fd)
                self._fd = None


# ── adapter router (adapters land in US3-US5) ───────────────────────────────


def dispatch_event(
    sink: StatusSinkConfig,
    event: dict[str, Any],
    fanout: StatusFanoutConfig,
    project_root: Path,
) -> "tuple[str, str]":
    """Route an event to the sink's adapter by type. Filled across US3-US5."""
    raise NotImplementedError(f"adapter for {sink.type!r} not implemented")


# ── CLI ─────────────────────────────────────────────────────────────────────

status_fanout_app = typer.Typer(
    help="Status-sink fanout: sweep events.jsonl and route to configured sinks.",
    no_args_is_help=True,
)


@status_fanout_app.command("tick")
def tick_cmd(
    dry_run: bool = typer.Option(
        False, "--dry-run", help="Preview per-sink matched counts; send nothing, advance no cursor."
    ),
) -> None:
    """Run one fanout pass over this project's events.jsonl."""
    from fno.config import load_settings

    root = paths.resolve_repo_root()
    settings = load_settings()
    result = run_tick(
        root,
        settings.status_sinks,
        settings.status_fanout,
        dry_run=dry_run,
    )
    if result.locked_out:
        typer.echo("status-fanout: another tick holds the lock; skipped")
        return
    if not result.sinks:
        typer.echo("status-fanout: no enabled sinks (no-op)")
        return
    verb = "would-send" if dry_run else "dispatched"
    for sr in result.sinks:
        sent = sr.matched if dry_run else sr.dispatched
        extra = " short-circuited" if sr.short_circuited else ""
        typer.echo(
            f"status-fanout: {sr.name} matched={sr.matched} {verb}={sent} "
            f"dropped={sr.dropped}{extra}"
        )
    if result.skipped_lines:
        typer.echo(f"status-fanout: skipped {result.skipped_lines} malformed line(s)")
