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
import string
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
    new_cursor: "Optional[tuple[str, int]]" = None


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


def _read_events(path: Path, since_ts: Optional[str]) -> "tuple[list[dict[str, Any]], int]":
    """Return (events with ts >= since_ts, malformed_line_count) from one file.

    The bound is INCLUSIVE so a boundary event sharing the cursor's second is
    still seen; the per-sink ``(ts, n)`` tiebreak decides whether it is new (see
    _run_locked). Streams line-by-line; a malformed / ts-less line is skipped and
    counted (digest.rs posture), never fatal.
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
                if since_ts is None or ev["ts"] >= since_ts:
                    events.append(ev)
    except FileNotFoundError:
        return [], 0
    except (OSError, UnicodeDecodeError):
        # A permission error or a corrupt (non-utf8) byte mid-read must not crash
        # the tick; return what parsed so far (the cursor simply does not advance
        # past the unread tail, which retries next tick).
        return events, skipped
    return events, skipped


def _first_ts(active: Path) -> Optional[str]:
    """The ts of the active file's first parseable line, or None if empty/absent.
    Used to decide whether the rotated ``.1`` could still hold un-cursored events."""
    try:
        with active.open("r", encoding="utf-8") as fh:
            for raw in fh:
                ev = _parse_line(raw)
                if ev is not None:
                    return str(ev["ts"])
    except FileNotFoundError:
        return None
    return None


def _stream_since(active: Path, since_ts: Optional[str]) -> "tuple[list[dict[str, Any]], int]":
    """All events with ts >= since_ts, draining the rotated ``.1`` first ONLY when
    the cursor predates the active file's first line (else ``.1`` is fully covered
    and re-scanning its up-to-8MB tail every tick is wasted IO). Rotated history is
    prepended so the returned list stays ts-ordered (both files are individually
    ordered and ``.1`` is strictly older)."""
    active_events, active_skipped = _read_events(active, since_ts)
    rotated = active.with_name(active.name + ".1")
    if not rotated.exists():
        return active_events, active_skipped
    active_first = _first_ts(active)
    # .1 only matters if the cursor is at/before the active file's first line;
    # once the cursor is inside the active file, the rotated tail is consumed.
    if since_ts is not None and active_first is not None and since_ts >= active_first:
        return active_events, active_skipped
    rotated_events, rotated_skipped = _read_events(rotated, since_ts)
    return rotated_events + active_events, active_skipped + rotated_skipped


def _eof_cursor(active: Path) -> "tuple[str, int]":
    """The fresh-sink starting cursor: (max_ts, count_of_events_at_max_ts) so no
    historical event is replayed AND a later event in the SAME second as EOF is
    still delivered (it lands at occurrence index >= count). ("", 0) for an
    empty/absent log means "deliver everything henceforth" (nothing to backfill)."""
    last_ts = ""
    count = 0
    try:
        with active.open("r", encoding="utf-8") as fh:
            for raw in fh:
                ev = _parse_line(raw)
                if ev is None:
                    continue
                ts = ev["ts"]
                if ts == last_ts:
                    count += 1
                elif ts > last_ts:
                    last_ts, count = ts, 1
                # ts < last_ts (clock skew): leave the max-ts count untouched.
    except FileNotFoundError:
        pass
    return last_ts, count


# ── cursor io (atomic) ──────────────────────────────────────────────────────


def _cursor_path(name: str, project_root: Optional[Path]) -> Path:
    return paths.status_sinks_dir(project_root) / f"{name}.cursor"


def _errors_path(name: str, project_root: Optional[Path]) -> Path:
    return paths.status_sinks_dir(project_root) / f"{name}.errors.jsonl"


def _read_cursor(name: str, project_root: Optional[Path]) -> "Optional[tuple[str, int]]":
    """Read a sink's ``(ts, n)`` cursor, or None if absent/unreadable/malformed
    (all treated as fresh - a harmless re-init at EOF)."""
    try:
        raw = _cursor_path(name, project_root).read_text(encoding="utf-8")
    except OSError:  # FileNotFoundError, PermissionError, ... - never crash the tick
        return None
    try:
        obj = json.loads(raw)
        return str(obj["ts"]), int(obj["n"])
    except (ValueError, KeyError, TypeError):
        return None  # a torn/legacy cursor reads as fresh (harmless re-init at EOF)


def _write_cursor(name: str, cursor: "tuple[str, int]", project_root: Optional[Path]) -> None:
    path = _cursor_path(name, project_root)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps({"ts": cursor[0], "n": cursor[1]}), encoding="utf-8")
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
    eof = _eof_cursor(active)  # (ts, count_at_ts) - the fresh-sink floor

    # Each sink's starting (ts, n) cursor: a fresh sink (no file) starts at EOF so
    # no history is replayed; an existing sink resumes from its stored cursor.
    fresh: dict[str, bool] = {}
    start: dict[str, tuple[str, int]] = {}
    for s in sinks:
        cur = _read_cursor(s.name, project_root)
        fresh[s.name] = cur is None
        start[s.name] = cur if cur is not None else eof

    # Read from the oldest cursor ts INCLUSIVE so every sink sees its own same-ts
    # boundary events; the per-sink (ts, n) tiebreak below decides what is new.
    min_ts = min(c[0] for c in start.values())
    events, skipped = _stream_since(active, min_ts)

    state = {s.name: SinkResult(name=s.name, new_cursor=start[s.name]) for s in sinks}

    # occurrence index of each event among its same-ts peers, in file order. The
    # ts is seconds-granularity, so two events routinely share one ts; (ts, idx)
    # is the stable identity a bare-ts cursor lacked (the same-second drop bug).
    occ: dict[str, int] = {}
    for event in events:
        ets = event["ts"]
        idx = occ.get(ets, 0)
        occ[ets] = idx + 1
        for s in sinks:
            st = state[s.name]
            if st.short_circuited:
                continue
            cts, cn = st.new_cursor  # type: ignore[misc]  # always a tuple here
            # Already processed: a strictly-older ts, or a same-ts occurrence the
            # cursor already advanced past.
            if ets < cts or (ets == cts and idx < cn):
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
                st.new_cursor = (ets, idx + 1)
            elif status == SHORT_CIRCUIT:
                # Hold the cursor: this event + everything after retries next tick.
                st.short_circuited = True
                _log_error(s.name, project_root, {
                    "sink": s.name, "event_ts": ets, "type": event.get("type"),
                    "reason": detail, "class": "short_circuit"})
            else:
                # DROPPED, or any unrecognized status -> drop + log and advance,
                # never a silent short-circuit-forever on a typo'd dispatcher.
                st.dropped += 1
                st.new_cursor = (ets, idx + 1)
                reason = detail if status == DROPPED else f"unknown dispatch status {status!r}"
                _log_error(s.name, project_root, {
                    "sink": s.name, "event_ts": ets, "type": event.get("type"),
                    "reason": reason, "class": "dropped"})

    # Persist advanced cursors, ISOLATED per sink: one sink's cursor-write failure
    # (disk full / perms) must not abort the others' persistence and drive a
    # silent re-delivery storm. A fresh sink persists its EOF floor even with zero
    # dispatch so the next tick never backfills.
    if not dry_run:
        for s in sinks:
            st = state[s.name]
            if st.dispatched or st.dropped or fresh[s.name]:
                try:
                    _write_cursor(s.name, st.new_cursor, project_root)  # type: ignore[arg-type]
                except OSError as exc:
                    _log_error(s.name, project_root, {
                        "sink": s.name, "reason": f"cursor write failed: {exc}",
                        "class": "cursor_write_failed"})

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

        try:
            self._path.parent.mkdir(parents=True, exist_ok=True)
            fd = os.open(self._path, os.O_CREAT | os.O_RDWR, 0o644)
        except OSError:
            # A read-only fs / permission error creating the lockfile: treat as
            # "cannot lock" (the tick skips as locked_out) rather than crashing.
            return False
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


# ── HTTP delivery (shared by the two webhook adapters) ──────────────────────

# Backoff schedule between retries (seconds); index by attempt number, clamped
# to the last entry. Small and bounded - the tick holds the per-project lock.
_BACKOFF = (1.0, 3.0)
_MAX_RETRY_AFTER = 30.0  # cap a server's Retry-After so one sink can't wedge a tick


def _sleep(seconds: float) -> None:
    import time

    time.sleep(seconds)


@dataclass
class _HttpResult:
    ok: bool
    status: Optional[int] = None  # None => connect-class (no HTTP response)
    retry_after: Optional[float] = None


def _post_json(url: str, body: dict[str, Any], timeout: float) -> _HttpResult:
    """POST a JSON body. A connect-class failure (timeout / DNS / refused) returns
    status=None; an HTTP error response returns its status code."""
    import urllib.error
    import urllib.request

    data = json.dumps(body).encode("utf-8")
    try:
        req = urllib.request.Request(
            url, data=data, method="POST",
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            code = resp.getcode()
            return _HttpResult(ok=200 <= code < 300, status=code)
    except urllib.error.HTTPError as e:
        ra = e.headers.get("Retry-After") if e.headers else None
        try:
            retry_after = float(ra) if ra is not None else None
        except (TypeError, ValueError):
            retry_after = None
        return _HttpResult(ok=False, status=e.code, retry_after=retry_after)
    except ValueError:
        # A malformed URL (e.g. missing scheme) raises ValueError from urlopen and
        # is NOT a URLError/OSError - a permanent client error, so drop (status 400)
        # rather than retry-forever as connect-class.
        return _HttpResult(ok=False, status=400)
    except (urllib.error.URLError, OSError):
        return _HttpResult(ok=False, status=None)  # connect-class


def _deliver(url: str, body: dict[str, Any], fanout: StatusFanoutConfig) -> "tuple[str, str]":
    """Retry/failure-class driver shared by the webhook adapters.

    - 4xx except 429  -> DROPPED immediately (permanent; advance past it).
    - connect-class / 5xx / 429 -> bounded retry, then SHORT_CIRCUIT (transient;
      hold the cursor and retry next tick). 429 honors Retry-After within budget.
    """
    attempts = max(1, fanout.retries + 1)
    result = _HttpResult(ok=False)
    for i in range(attempts):
        result = _post_json(url, body, float(fanout.http_timeout_secs))
        if result.ok:
            return DELIVERED, ""
        if result.status is not None and 400 <= result.status < 500 and result.status != 429:
            return DROPPED, f"http {result.status}"
        if i < attempts - 1:
            if result.status == 429 and result.retry_after:
                delay = min(result.retry_after, _MAX_RETRY_AFTER)
            else:
                delay = _BACKOFF[min(i, len(_BACKOFF) - 1)]
            _sleep(delay)
    reason = f"http {result.status}" if result.status else "connect-class"
    return SHORT_CIRCUIT, f"exhausted retries ({reason})"


def _resolve_url(sink: StatusSinkConfig) -> "tuple[Optional[str], Optional[str]]":
    """Resolve a webhook URL from ``url`` or ``url_env``. Returns (url, error);
    a missing env secret is short-circuit-worthy (fixable), not a hard drop."""
    if sink.url:
        return sink.url, None
    if sink.url_env:
        val = os.environ.get(sink.url_env)
        if val:
            return val, None
        return None, f"url_env {sink.url_env} unset"
    return None, "no url configured"


# ── adapter router + adapters ───────────────────────────────────────────────


def dispatch_event(
    sink: StatusSinkConfig,
    event: dict[str, Any],
    fanout: StatusFanoutConfig,
    project_root: Path,
) -> "tuple[str, str]":
    """Route an event to the sink's adapter by type."""
    if sink.type == "json-webhook":
        return _dispatch_json_webhook(sink, event, fanout)
    if sink.type == "text-webhook":
        return _dispatch_text_webhook(sink, event, fanout)
    if sink.type == "backlog-progress":
        return _dispatch_backlog_progress(sink, event, project_root)
    return DROPPED, f"unknown sink type {sink.type}"


def _cloudevents_wrap(event: dict[str, Any]) -> dict[str, Any]:
    """Wrap the canonical event in the 5-field CloudEvents envelope. `id` is
    derived from run+ts+type so a re-delivered event carries a stable id (a
    receiver can dedupe on it)."""
    return {
        "id": f"{event.get('run', '')}:{event.get('ts', '')}:{event.get('type', '')}",
        "source": event.get("source", "fno"),
        "type": event.get("type", ""),
        "time": event.get("ts", ""),
        "data": event,
    }


def _dispatch_json_webhook(
    sink: StatusSinkConfig, event: dict[str, Any], fanout: StatusFanoutConfig
) -> "tuple[str, str]":
    """POST the canonical event JSON verbatim (optionally CloudEvents-wrapped).
    The escape hatch: n8n / Zapier / Sheets glue / any receiver maps the raw
    payload."""
    url, err = _resolve_url(sink)
    if url is None:
        return SHORT_CIRCUIT, err or "no url"
    body = _cloudevents_wrap(event) if sink.cloudevents else event
    return _deliver(url, body, fanout)


class _EventFormatter(string.Formatter):
    """Template renderer over an event dict. Unlike bare ``str.format``:
    ``{data.reason}`` does *dict item* traversal (not attribute access, which
    raises on a dict), and any missing field renders empty instead of raising."""

    def get_field(self, field_name: str, args: Any, kwargs: Any) -> "tuple[Any, str]":
        first, _, rest = field_name.partition(".")
        obj: Any = kwargs.get(first, "") if isinstance(kwargs, dict) else ""
        for part in (rest.split(".") if rest else []):
            obj = obj.get(part, "") if isinstance(obj, dict) else ""
        return obj, first

    def get_value(self, key: Any, args: Any, kwargs: Any) -> Any:
        if isinstance(key, int):
            return ""  # positional refs unsupported; never crash
        return kwargs.get(key, "") if isinstance(kwargs, dict) else ""

    def format_field(self, value: Any, format_spec: str) -> str:
        return str(super().format_field("" if value is None else value, format_spec))


def _render_template(template: Optional[str], event: dict[str, Any]) -> str:
    try:
        return _EventFormatter().vformat(template or "", (), event)
    except (ValueError, KeyError, IndexError):
        # A malformed template (e.g. an unbalanced brace) degrades to raw text
        # rather than crashing the tick.
        return template or ""


def _dispatch_text_webhook(
    sink: StatusSinkConfig, event: dict[str, Any], fanout: StatusFanoutConfig
) -> "tuple[str, str]":
    """Render ``template`` against the event and POST ``{field: rendered}``. One
    adapter serves Discord (``content``) / Slack-incoming (``text``) / ntfy via
    the configurable ``field``. A Discord-shaped post (``field == "content"``)
    sends ``allowed_mentions: {"parse": []}`` so a worker-influenced reason
    containing ``@everyone`` cannot ping the server."""
    url, err = _resolve_url(sink)
    if url is None:
        return SHORT_CIRCUIT, err or "no url"
    body: dict[str, Any] = {sink.field: _render_template(sink.template, event)}
    if sink.field == "content":
        body["allowed_mentions"] = {"parse": []}
    return _deliver(url, body, fanout)


def _progress_line(event: dict[str, Any]) -> str:
    """One-line human summary of a task-boundary event for the node/plan."""
    kind = event.get("type", "")
    outcome = event.get("outcome")
    reason = event.get("data", {}).get("reason") if isinstance(event.get("data"), dict) else None
    bits = [kind]
    if outcome:
        bits.append(str(outcome))
    if reason:
        bits.append(str(reason))
    return " - ".join(bits)


def _append_plan_progress(plan_path: str, text: str, project_root: Path) -> None:
    """Append ``- <ts> <text>`` under a ``## Progress`` heading in the node's plan
    doc. Body-only: NEVER touches frontmatter (the ship-gate stamp owns that).
    Best-effort - a missing/unresolvable path or any IO error is skipped silently
    (a vault miss is never a delivery failure). The heading is created at EOF on
    first use; since this adapter is the sole body-appender, Progress stays the
    trailing section and later notes append beneath it."""
    if not plan_path:
        return
    p = Path(plan_path)
    if not p.is_absolute():
        p = project_root / plan_path
    try:
        p = p.resolve()
        if not p.is_file():
            return
        content = p.read_text(encoding="utf-8")
    except OSError:
        return
    line = f"- {text}"
    if "## Progress" in content:
        new = content.rstrip("\n") + "\n" + line + "\n"
    else:
        new = content.rstrip("\n") + "\n\n## Progress\n\n" + line + "\n"
    try:
        p.write_text(new, encoding="utf-8")
    except OSError:
        return


def _dispatch_backlog_progress(
    sink: StatusSinkConfig, event: dict[str, Any], project_root: Path
) -> "tuple[str, str]":
    """On ``task_done`` / ``run_summary`` carrying a ``node``: append a timestamped
    progress note to the graph node AND to its plan doc's ``## Progress`` section.
    Other kinds / node-less events are a no-op (advance the cursor)."""
    if event.get("type") not in ("task_done", "run_summary"):
        return DELIVERED, ""
    node_id = event.get("node")
    if not node_id:
        return DELIVERED, ""

    from fno import paths as _paths
    from fno.graph.store import append_progress_note

    text = _progress_line(event)
    note = {"ts": event.get("ts", ""), "text": text}
    try:
        found, plan_path = append_progress_note(_paths.graph_json(), node_id, note)
    except Exception as exc:  # a graph write failure is a real (droppable) failure
        return DROPPED, f"graph note failed: {exc}"
    if not found:
        return DROPPED, f"node {node_id} not found in graph"
    # Plan-doc append is best-effort and never fails the delivery.
    if plan_path:
        _append_plan_progress(plan_path, f"{note['ts']} {text}", project_root)
    return DELIVERED, ""


# ── CLI ─────────────────────────────────────────────────────────────────────

status_fanout_app = typer.Typer(
    help="Status-sink fanout: sweep events.jsonl and route to configured sinks.",
    no_args_is_help=True,
)


@status_fanout_app.command("tick")
def tick_cmd(
    dry_run: bool = typer.Option(
        False, "--dry-run", "-N",
        help="Preview per-sink matched counts; send nothing, advance no cursor.",
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
