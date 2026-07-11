"""``fno agents peek <handle>`` — the read-only twin of ``fno mail send``.

Reply is agent-native (``mail send`` resolves ``<handle>`` across every live
source). Observe was tribal knowledge (``agents logs`` is registry-only; a live
codex thread or unrostered ``claude --bg`` session had no single observe verb).
``peek`` closes the asymmetry: same union resolver, read instead of write.

Two data paths, tried in order (design x-05da):

1. **Status stream (fast-path, opportunistic).** The normalized
   ``task_started`` / ``task_done`` / ``blocked`` / ``run_summary`` events a
   worker emits to ``events.jsonl``. Cheap, cross-harness. Not shipped by every
   worker yet, so absent → fall through with no error.
2. **Transcript tail (fallback, ships now).** Resolve the handle to its
   harness's on-disk transcript and tail the last N records. Works for every
   worker today.

The per-harness on-disk shape differs (claude/codex = one JSONL; opencode = a
per-message dir), so the extensible seam is ``recent_records`` dispatching on
``agent``. claude + codex arms ship here; opencode is a fast-follow arm.

Read-only invariant: peek opens files for read and polls stat for ``--follow``.
It never writes ``events.jsonl``, the peer transcript, the registry, or a
mailbox — observing must not perturb the observed.
"""
from __future__ import annotations

import dataclasses
import json
import sys
import time
from pathlib import Path
from typing import Callable, Optional

EXIT_OK = 0
EXIT_UNSUPPORTED = 1  # known peer, no reader arm — distinct from "not found"
EXIT_NOT_FOUND = 13  # parity with mail send's unresolvable-handle exit

# Harnesses with a recent_records arm today. opencode is the fast-follow arm.
_SUPPORTED_AGENTS = frozenset({"claude", "codex"})

_STATUS_KINDS = frozenset(
    {"task_started", "task_done", "blocked", "run_summary"}
)


@dataclasses.dataclass
class Record:
    """One rendered transcript turn: a role and its human-readable text."""

    role: str
    text: str


class ObserveUnsupported(Exception):
    """Handle resolved to an agent with no ``recent_records`` arm (AC2-ERR)."""

    def __init__(self, agent: str) -> None:
        super().__init__(agent)
        self.agent = agent


# --------------------------------------------------------------------------
# Record extraction (harness-agnostic where the block shape allows)
# --------------------------------------------------------------------------


def _extract_text(content: object) -> str:
    """Flatten a message ``content`` to legible text.

    Handles a bare string, claude blocks (``text`` / ``tool_use``), and codex
    blocks (``input_text`` / ``output_text`` — both carry a ``text`` field).
    ``thinking`` and ``tool_result`` bodies are dropped as observe-noise;
    ``tool_use`` renders a compact marker so the peer's actions stay visible.
    """
    if isinstance(content, str):
        return content.strip()
    if not isinstance(content, list):
        return ""
    parts: list[str] = []
    for block in content:
        if not isinstance(block, dict):
            continue
        if isinstance(block.get("text"), str):
            parts.append(block["text"])
        elif block.get("type") == "tool_use":
            parts.append(f"[tool_use: {block.get('name', '?')}]")
    return " ".join(p.strip() for p in parts if p.strip())


def _parse_claude_record(rec: dict) -> Optional[Record]:
    """A claude transcript ``user``/``assistant`` line → Record, else None."""
    if rec.get("type") not in ("user", "assistant"):
        return None
    msg = rec.get("message")
    if not isinstance(msg, dict):
        return None
    text = _extract_text(msg.get("content"))
    if not text:
        return None
    return Record(role=str(msg.get("role") or rec.get("type")), text=text)


def _parse_codex_record(rec: dict) -> Optional[Record]:
    """A codex rollout ``response_item`` message line → Record, else None."""
    if rec.get("type") != "response_item":
        return None
    payload = rec.get("payload")
    if not isinstance(payload, dict) or payload.get("type") != "message":
        return None
    text = _extract_text(payload.get("content"))
    if not text:
        return None
    return Record(role=str(payload.get("role") or "?"), text=text)


def _records_from_jsonl(
    path: Path, n: Optional[int], parse: Callable[[dict], Optional[Record]]
) -> list[Record]:
    """Parse the last ``n`` renderable records from a JSONL transcript.

    Streams line-by-line into a bounded deque so memory stays O(n). A torn or
    non-JSON line (mid-write tail, AC2-EDGE) is skipped, never raised. ``n`` of
    0 or negative returns ``[]``; ``None`` returns every record.
    """
    import collections

    if n is not None and n <= 0:
        return []
    dq: "collections.deque[Record]" = collections.deque(maxlen=n)
    try:
        with path.open("r", encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    rec = json.loads(line)
                except (ValueError, UnicodeDecodeError):
                    continue  # torn/partial trailing record: skip
                if not isinstance(rec, dict):
                    continue
                record = parse(rec)
                if record is not None:
                    dq.append(record)
    except OSError:
        return []
    return list(dq)


def _codex_rollout_path(
    session_id: str, codex_sessions_dir: Optional[Path]
) -> Optional[Path]:
    """Locate the codex rollout whose ``session_meta`` id matches ``session_id``.

    Re-uses discover's rollout scan + meta parse (single source of truth for the
    codex layout) rather than threading a path through the dedup pipeline.
    """
    from fno.agents.discover import _codex_meta, default_codex_sessions_dir

    root = codex_sessions_dir or default_codex_sessions_dir()
    dated: list[tuple[float, Path]] = []
    try:
        for path in root.rglob("rollout-*.jsonl"):
            try:
                dated.append((path.stat().st_mtime, path))
            except OSError:
                continue  # vanished mid-scan: skip this file, never abort the scan
    except OSError:
        return None
    rollouts = [p for _mt, p in sorted(dated, key=lambda t: t[0], reverse=True)]
    for path in rollouts:
        meta = _codex_meta(path)
        if meta is not None and meta[0] == session_id:
            return path
    return None


def recent_records(
    agent: str,
    session_id: str,
    cwd: str,
    n: Optional[int],
    *,
    projects_root: Optional[Path] = None,
    codex_sessions_dir: Optional[Path] = None,
) -> list[Record]:
    """The per-harness reader seam (Locked Decision 3).

    Dispatches on ``agent`` and returns a uniform ``Record`` list so the command
    body never special-cases a harness. An empty list means "resolved, nothing
    to show yet". An unregistered harness raises ``ObserveUnsupported`` (the
    command turns that into a legible exit-1, distinct from the exit-13 miss).
    """
    if agent == "claude":
        from fno.provenance.resolver import resolve_transcript

        rt = resolve_transcript(
            "claude", session_id, cwd, projects_root=projects_root
        )
        if not rt.resolved or not rt.transcript_path:
            return []
        return _records_from_jsonl(
            Path(rt.transcript_path), n, _parse_claude_record
        )
    if agent == "codex":
        path = _codex_rollout_path(session_id, codex_sessions_dir)
        if path is None:
            return []
        return _records_from_jsonl(path, n, _parse_codex_record)
    raise ObserveUnsupported(agent)


# --------------------------------------------------------------------------
# Status-stream fast-path (US4) — dual-envelope, opportunistic
# --------------------------------------------------------------------------


def _status_event_line(rec: dict) -> Optional[tuple[str, str]]:
    """Parse one events.jsonl record into ``(kind, id)`` if it is a status event.

    Accepts BOTH envelope shapes (x-2901 split-brain, a permanent superset):
    Python ``{type, data:{...}}`` and Rust ``{kind, ...flat}``. Returns None for
    a record that is neither — the caller skips it and falls through.
    """
    if not isinstance(rec, dict):
        return None
    kind = rec.get("kind") or rec.get("type")
    if kind not in _STATUS_KINDS:
        return None
    data = rec.get("data") if isinstance(rec.get("data"), dict) else rec
    ident = ""
    for field in ("short_id", "session_id", "source", "worker"):
        val = data.get(field) or rec.get(field)
        if isinstance(val, str) and val:
            ident = val
            break
    return kind, ident


def _status_events(
    events_path: Optional[Path], short_id: str, session_id: str
) -> list[str]:
    """Rendered status lines for this session, or ``[]`` to fall through.

    A record parseable as neither envelope shape is skipped (never presents a
    partial status view as complete). Missing/rotated file → ``[]``.
    """
    if events_path is None:
        return []
    wants = {v for v in (short_id, session_id, f"worker:{short_id}") if v}
    lines: list[str] = []
    try:
        with events_path.open("r", encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    rec = json.loads(line)
                except (ValueError, UnicodeDecodeError):
                    continue
                parsed = _status_event_line(rec)
                if parsed is None:
                    continue
                kind, ident = parsed
                if wants and ident and ident not in wants:
                    # scope to this session; unscoped events with no id match all
                    if not any(w in ident for w in wants):
                        continue
                lines.append(f"  [{kind}] {ident}".rstrip())
    except OSError:
        return []
    return lines


# --------------------------------------------------------------------------
# Follow loop — read-only poll, exits on rotation / not-live / Ctrl-C
# --------------------------------------------------------------------------


def _read_complete_lines(fh) -> list[bytes]:
    """Read whole (newline-terminated) lines from a binary handle at EOF.

    A concurrent writer can leave a partial trailing line (bytes flushed before
    the ``\\n``). Consuming it now, failing to parse, and reading the remainder
    next poll would drop BOTH halves of one record. Instead we stop at the first
    non-newline-terminated read and ``seek`` back over it, so the next poll
    re-reads that record whole once the writer completes it (plan Concurrency
    invariant: "next poll picks up the completed record").
    """
    lines: list[bytes] = []
    while True:
        pos = fh.tell()
        line = fh.readline()
        if not line:
            break  # genuine EOF, nothing buffered
        if not line.endswith(b"\n"):
            fh.seek(pos)  # partial line: rewind, wait for the writer to finish it
            break
        lines.append(line)
    return lines


def _follow_records(
    path: Path,
    parse: Callable[[dict], Optional[Record]],
    stdout,
    stderr,
    *,
    is_live: Optional[Callable[[], bool]] = None,
    json_out: bool = False,
    poll_interval: float = 0.5,
    idle_polls_before_liveness: int = 4,
) -> None:
    """Stream new parsed records as the transcript grows.

    Reads only complete lines (a mid-write partial waits for its completion,
    never corrupts a record). ``json_out`` keeps followed records in JSON-Lines
    so a stream started with ``--json`` stays parseable. Exits cleanly when the
    file rotates/disappears
    (peer ended + cleaned up) or, after a stretch of no growth, when
    ``is_live()`` reports the peer gone (AC1-FR: no infinite spin).
    KeyboardInterrupt is trapped by the caller.
    """
    try:
        initial = path.stat()
    except OSError:
        stderr.write(f"transcript disappeared: {path}\n")
        return
    idle = 0
    with path.open("rb") as fh:
        fh.seek(0, 2)  # end
        while True:
            new_lines = _read_complete_lines(fh)
            if new_lines:
                idle = 0
                for raw in new_lines:
                    stripped = raw.strip()
                    if not stripped:
                        continue
                    try:
                        rec = json.loads(stripped.decode("utf-8"))
                    except (ValueError, UnicodeDecodeError):
                        continue
                    if isinstance(rec, dict):
                        record = parse(rec)
                        if record is not None:
                            _emit_record(stdout, record, json_out)
                            if hasattr(stdout, "flush"):
                                stdout.flush()
                continue
            try:
                st = path.stat()
            except OSError:
                stderr.write(f"transcript disappeared: {path}\n")
                return
            if st.st_ino != initial.st_ino or st.st_size < fh.tell():
                stderr.write(f"transcript rotated: {path}\n")
                return
            idle += 1
            if (
                is_live is not None
                and idle >= idle_polls_before_liveness
                and not is_live()
            ):
                stderr.write("peer ended; no further activity\n")
                return
            time.sleep(poll_interval)


# --------------------------------------------------------------------------
# Rendering + command entrypoint
# --------------------------------------------------------------------------


def _render(record: Record) -> str:
    return f"{record.role}: {record.text}"


def _emit_record(out, record: Record, json_out: bool) -> None:
    """Write one record in the caller's mode. ``--json`` stays JSON-Lines
    everywhere (initial tail AND followed records) so a consumer never trips
    over a human line mid-stream."""
    if json_out:
        out.write(json.dumps({"role": record.role, "text": record.text}) + "\n")
    else:
        out.write(_render(record) + "\n")


def _emit_no_activity(out, json_out: bool) -> None:
    """Emit the idle state as a JSON status row under ``--json`` (else a line),
    so ``peek --json`` on an idle peer is still parseable JSON-Lines."""
    if json_out:
        out.write(json.dumps({"status": "no activity yet"}) + "\n")
    else:
        out.write("no activity yet\n")


def _default_resolve(handle: str):
    from fno.agents.discover import resolve_or_suggest

    return resolve_or_suggest(handle)


def peek(
    handle: str,
    *,
    lines: int = 15,
    follow: bool = False,
    json_out: bool = False,
    stdout=None,
    stderr=None,
    resolve: Optional[Callable[[str], tuple]] = None,
    projects_root: Optional[Path] = None,
    codex_sessions_dir: Optional[Path] = None,
    events_path: Optional[Path] = None,
    is_live: Optional[Callable[[], bool]] = None,
) -> int:
    """Observe a peer by handle. Returns the process exit code.

    Every terminal state is legible (AC1-UI): ``peer not found`` (13),
    ``observe not yet supported`` (1), ``no activity yet`` (0), or a header plus
    records / status lines. There is no blank exit-0 a caller could misread as
    "idle".
    """
    out = stdout if stdout is not None else sys.stdout
    err = stderr if stderr is not None else sys.stderr
    resolver = resolve if resolve is not None else _default_resolve

    session, suggestions = resolver(handle)
    if session is None:
        err.write(f"peer not found: {handle}\n")
        if suggestions:
            err.write(f"did you mean: {', '.join(suggestions)}\n")
        return EXIT_NOT_FOUND

    agent = getattr(session, "agent", "claude")
    session_id = getattr(session, "session_id", "")
    short_id = getattr(session, "short_id", "")
    cwd = getattr(session, "cwd", "")

    if not json_out:
        out.write(
            f"peer {handle}: agent={agent} short_id={short_id} cwd={cwd}\n"
        )

    # Fast-path: prefer normalized status events when present.
    status = _status_events(events_path, short_id, session_id)
    if status:
        if json_out:
            for line in status:
                out.write(json.dumps({"status": line.strip()}) + "\n")
        else:
            out.write("\n".join(status) + "\n")
        return EXIT_OK

    try:
        records = recent_records(
            agent,
            session_id,
            cwd,
            lines,
            projects_root=projects_root,
            codex_sessions_dir=codex_sessions_dir,
        )
    except ObserveUnsupported as exc:
        err.write(f"observe not yet supported for {exc.agent}\n")
        return EXIT_UNSUPPORTED

    if not records and not follow:
        _emit_no_activity(out, json_out)
        return EXIT_OK

    for rec in records:
        _emit_record(out, rec, json_out)
    if not records:
        _emit_no_activity(out, json_out)

    if follow:
        # Re-resolve the transcript path for the follow loop (records above came
        # from the same reader; codex/claude both back onto a single JSONL).
        path = _follow_target(
            agent, session_id, cwd, projects_root, codex_sessions_dir
        )
        if path is None:
            return EXIT_OK
        try:
            _follow_records(
                path,
                _parse_claude_record if agent == "claude" else _parse_codex_record,
                out,
                err,
                is_live=is_live,
                json_out=json_out,
            )
        except KeyboardInterrupt:
            return EXIT_OK  # AC1-FR: clean Ctrl-C, no traceback
    return EXIT_OK


def _follow_target(
    agent: str,
    session_id: str,
    cwd: str,
    projects_root: Optional[Path],
    codex_sessions_dir: Optional[Path],
) -> Optional[Path]:
    """The single JSONL file to tail for ``--follow`` (claude/codex only)."""
    if agent == "claude":
        from fno.provenance.resolver import resolve_transcript

        rt = resolve_transcript(
            "claude", session_id, cwd, projects_root=projects_root
        )
        return Path(rt.transcript_path) if rt.resolved and rt.transcript_path else None
    if agent == "codex":
        return _codex_rollout_path(session_id, codex_sessions_dir)
    return None
