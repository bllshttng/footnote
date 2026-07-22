"""Canonical JSONL bus log: versioned envelope + locked writer + reader.

Write discipline (locked decision 7, hardened): each append takes an
``flock`` on a sidecar lockfile (``messages.jsonl.lock``), then writes the
whole line with ``O_APPEND``. ``O_APPEND`` alone fixes the offset race but
does NOT guarantee a multi-KB line lands without interleaving on a regular
file (the POSIX small-write guarantee is for pipes; the macOS threshold is
tiny). Lock + O_APPEND is bulletproof at any body size and uncontended at
agent-messaging rates. Rotation is size-triggered (``messages.jsonl`` ->
``messages.jsonl.1`` -> ``.2`` ...), bounded by a retention count.

The log is append-only: no in-place mutation. Corrections and delivery-state
changes are new envelopes, never edits.
"""
from __future__ import annotations

import fcntl
import json
import os
import secrets
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterator, Optional


ENVELOPE_VERSION = 1

# Size-triggered rotation. A segment is rolled once it reaches this many bytes
# (checked before each append, under the lock). Env overrides exist for tests
# and operators; a malformed override degrades to the default rather than raising.
_DEFAULT_MAX_BYTES = 5 * 1024 * 1024  # 5 MB
_DEFAULT_RETAIN = 5  # rotated segments kept; cursors must resolve into these


def _max_bytes() -> int:
    raw = os.environ.get("FNO_BUS_MAX_BYTES")
    if raw:
        try:
            v = int(raw)
            if v > 0:
                return v
        except ValueError:
            pass
    return _DEFAULT_MAX_BYTES


def _retain() -> int:
    raw = os.environ.get("FNO_BUS_RETAIN")
    if raw:
        try:
            v = int(raw)
            if v >= 1:
                return v
        except ValueError:
            pass
    return _DEFAULT_RETAIN


# ---------------------------------------------------------------------------
# Envelope (versioned)
# ---------------------------------------------------------------------------

@dataclass
class Envelope:
    """One line in the bus log. ``from_`` serializes to the canonical ``from`` key.

    ``from`` and ``to`` are the addresses (registry names, or a project name in
    ``--to-project`` durable mode). ``provider_from``/``provider_to`` are
    metadata-only tags for transport selection and audit, never for addressing.
    Reply correlation uses ``request_id``/``in_reply_to`` exclusively.
    ``meta`` carries inbox-specific passthrough (refs, persist_to_memory) so the
    converged log preserves triage->graph provenance without polluting the
    canonical address/correlation fields.
    """

    id: str
    thread: str
    from_: str
    to: str
    kind: str
    body: str
    ts: str
    v: int = ENVELOPE_VERSION
    provider_from: Optional[str] = None
    provider_to: Optional[str] = None
    request_id: Optional[str] = None
    in_reply_to: Optional[str] = None
    delivery: Optional[str] = None
    meta: dict = field(default_factory=dict)
    # Addressed-delivery enrichment (Group 1, ab-ba91b807 / cv-d54ddd45). All
    # optional and omitted from the line when unset, so pre-existing lines are
    # byte-unchanged and old lines still parse (LD11 additive read).
    #  - from_session: the sender's session id, used to exclude the sender on a
    #    to_kind=project broadcast read (you never drain your own broadcast).
    #  - from_model:   the sender's model, surfaced in the render/projection.
    #  - to_kind:      addressing discriminator: "name" | "session" | "project".
    from_session: Optional[str] = None
    from_model: Optional[str] = None
    to_kind: Optional[str] = None

    @classmethod
    def new(
        cls,
        *,
        from_: str,
        to: str,
        kind: str,
        body: str,
        id: Optional[str] = None,
        thread: Optional[str] = None,
        ts: Optional[str] = None,
        provider_from: Optional[str] = None,
        provider_to: Optional[str] = None,
        request_id: Optional[str] = None,
        in_reply_to: Optional[str] = None,
        delivery: Optional[str] = None,
        meta: Optional[dict] = None,
        from_session: Optional[str] = None,
        from_model: Optional[str] = None,
        to_kind: Optional[str] = None,
    ) -> "Envelope":
        mid = id or new_msg_id()
        return cls(
            id=mid,
            thread=thread or mid,  # a root message threads under its own id
            from_=from_,
            to=to,
            kind=kind,
            body=body,
            ts=ts or _now_iso(),
            provider_from=provider_from,
            provider_to=provider_to,
            request_id=request_id,
            in_reply_to=in_reply_to,
            delivery=delivery,
            meta=dict(meta or {}),
            from_session=from_session,
            from_model=from_model,
            to_kind=to_kind,
        )


def new_msg_id() -> str:
    """Generate a 'msg-XXXXXX' id (6 hex chars), matching the inbox store."""
    return "msg-" + secrets.token_hex(3)


def _now_iso() -> str:
    return datetime.now(tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# Canonical key order. Always-present keys first, then optional tags, then body
# last (bodies can be large; keeping them last keeps the line head scannable).
_ALWAYS = ("v", "id", "ts", "thread", "from", "to", "kind")
_OPTIONAL = (
    "provider_from", "provider_to", "request_id", "in_reply_to", "delivery",
    "from_session", "from_model", "to_kind", "meta",
)


def to_json_line(env: Envelope) -> str:
    """Serialize an envelope to a single JSON line (no trailing newline).

    Single serializer for this surface (Python CLI). Optional tags are omitted
    when unset so lines stay clean; readers tolerate their absence.
    """
    obj: dict[str, object] = {
        "v": env.v,
        "id": env.id,
        "ts": env.ts,
        "thread": env.thread,
        "from": env.from_,
        "to": env.to,
        "kind": env.kind,
    }
    if env.provider_from:
        obj["provider_from"] = env.provider_from
    if env.provider_to:
        obj["provider_to"] = env.provider_to
    if env.request_id:
        obj["request_id"] = env.request_id
    if env.in_reply_to:
        obj["in_reply_to"] = env.in_reply_to
    if env.delivery:
        obj["delivery"] = env.delivery
    if env.from_session:
        obj["from_session"] = env.from_session
    if env.from_model:
        obj["from_model"] = env.from_model
    if env.to_kind:
        obj["to_kind"] = env.to_kind
    if env.meta:
        obj["meta"] = env.meta
    obj["body"] = env.body
    return json.dumps(obj, ensure_ascii=False, separators=(",", ":"))


def from_json_line(line: str) -> Envelope:
    """Parse one JSON line into an Envelope. Raises ValueError on bad shape."""
    obj = json.loads(line)
    if not isinstance(obj, dict):
        raise ValueError("envelope line is not a JSON object")
    # Required address/identity fields. Missing any -> malformed.
    for required in ("id", "from", "to", "kind"):
        if required not in obj:
            raise ValueError(f"envelope missing required field {required!r}")
    return Envelope(
        id=str(obj["id"]),
        thread=str(obj.get("thread", obj["id"])),
        from_=str(obj["from"]),
        to=str(obj["to"]),
        kind=str(obj["kind"]),
        body=str(obj.get("body", "")),
        ts=str(obj.get("ts", "")),
        v=int(obj.get("v", ENVELOPE_VERSION)),
        provider_from=obj.get("provider_from"),
        provider_to=obj.get("provider_to"),
        request_id=obj.get("request_id"),
        in_reply_to=obj.get("in_reply_to"),
        delivery=obj.get("delivery"),
        meta=_meta if isinstance((_meta := obj.get("meta")), dict) else {},
        from_session=obj.get("from_session"),
        from_model=obj.get("from_model"),
        to_kind=obj.get("to_kind"),
    )


# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------

def bus_log_path() -> Path:
    """Path to the live log segment (``<bus_dir>/messages.jsonl``)."""
    from fno import paths
    return paths.bus_dir() / "messages.jsonl"


def _lock_path() -> Path:
    return Path(str(bus_log_path()) + ".lock")


def _segment_paths_oldest_first(live: Path) -> list[Path]:
    """Return retained segments oldest -> newest: ``.N`` (high N) ... ``.1``, live."""
    rotated: list[tuple[int, Path]] = []
    parent = live.parent
    if parent.exists():
        prefix = live.name + "."
        for p in parent.iterdir():
            if p.name.startswith(prefix):
                suffix = p.name[len(prefix):]
                if suffix.isdigit():
                    rotated.append((int(suffix), p))
    rotated.sort(key=lambda t: t[0], reverse=True)  # oldest (highest N) first
    out = [p for _, p in rotated]
    if live.exists():
        out.append(live)
    return out


# ---------------------------------------------------------------------------
# Locked append + rotation
# ---------------------------------------------------------------------------

class _Flock:
    """Context manager holding an exclusive flock on the sidecar lockfile.

    The lockfile is separate from the log itself so the lock survives a
    rotation rename of ``messages.jsonl``.
    """

    def __init__(self, lock_path: Path):
        self._lock_path = lock_path
        self._fd: Optional[int] = None

    def __enter__(self) -> "_Flock":
        self._lock_path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(str(self._lock_path), os.O_CREAT | os.O_RDWR, 0o644)
        try:
            # If flock raises (e.g. KeyboardInterrupt, EINTR) __exit__ is never
            # called because __enter__ did not complete; close the fd here so it
            # is not leaked.
            fcntl.flock(fd, fcntl.LOCK_EX)
        except BaseException:
            os.close(fd)
            raise
        self._fd = fd
        return self

    def __exit__(self, *exc) -> None:
        if self._fd is not None:
            try:
                fcntl.flock(self._fd, fcntl.LOCK_UN)
            finally:
                os.close(self._fd)
                self._fd = None


def _rotate_locked(live: Path) -> None:
    """Shift segments: drop the oldest beyond retention, then live -> .1.

    Caller MUST hold the sidecar flock. No-op if the live segment is absent.
    """
    if not live.exists():
        return
    retain = _retain()
    # Drop the oldest segment that would fall outside retention.
    oldest = Path(f"{live}.{retain}")
    if oldest.exists():
        try:
            oldest.unlink()
        except OSError:
            pass
    # Shift .{n} -> .{n+1} from high to low so we never clobber.
    for n in range(retain - 1, 0, -1):
        src = Path(f"{live}.{n}")
        if src.exists():
            os.replace(str(src), f"{live}.{n + 1}")
    os.replace(str(live), f"{live}.1")


def append(env: Envelope) -> None:
    """Append one envelope to the log under the sidecar flock.

    Rotation is checked (and performed) under the same lock before the write, so
    concurrent APPENDERS never race on the size check or interleave a line. The
    lock serializes writers only; lock-free readers may transiently miss the
    just-renamed live->.1 segment during a rotation (the reader enumerates
    segments and checks ``live.exists()`` without the lock). That window is
    covered by the cursor fallback: a cursor whose message-id is not found in the
    retained scan rescans all segments rather than declaring loss, so a message
    is at most delayed by one drain cycle, never dropped.
    """
    live = bus_log_path()
    line = to_json_line(env) + "\n"
    data = line.encode("utf-8")
    with _Flock(_lock_path()):
        try:
            if live.exists() and live.stat().st_size >= _max_bytes():
                _rotate_locked(live)
        except OSError:
            # A stat failure must not lose the message; fall through to append.
            pass
        live.parent.mkdir(parents=True, exist_ok=True)
        # 0o600: the log holds message bodies; on a single global bus the
        # filesystem mode is the backstop behind the mediated read, so it is
        # owner-only (Group 1 privacy hardening, ab-ba91b807). umask may narrow
        # this further but never widens it. O_CREAT's mode applies only on
        # creation, so a segment created at 0o644 before this change would keep
        # appending bodies group/other-readable; tighten an existing segment
        # too. Best-effort: a chmod failure must never lose the message.
        if live.exists():
            try:
                os.chmod(str(live), 0o600)
            except OSError:
                pass
        fd = os.open(str(live), os.O_CREAT | os.O_WRONLY | os.O_APPEND, 0o600)
        try:
            # os.write may short-write; loop so a partial write never leaves a
            # truncated (corrupt) JSONL line. Under O_APPEND + the flock these
            # writes stay contiguous.
            written = 0
            while written < len(data):
                n = os.write(fd, data[written:])
                if n == 0:
                    raise OSError("bus log: os.write returned 0 bytes")
                written += n
        finally:
            os.close(fd)


# ---------------------------------------------------------------------------
# Reader (skips malformed lines)
# ---------------------------------------------------------------------------

def iter_messages(*, warn: bool = True) -> Iterator[Envelope]:
    """Yield every retained envelope oldest -> newest, skipping malformed lines.

    A corrupt line is skipped with a stderr warning (AC5-ERR); subsequent valid
    messages are still produced. Reads span all retained rotated segments plus
    the live segment, so a cursor keyed by message-id resolves across rotations.
    """
    live = bus_log_path()
    for seg in _segment_paths_oldest_first(live):
        try:
            with seg.open("r", encoding="utf-8") as f:
                for lineno, raw in enumerate(f, start=1):
                    raw = raw.rstrip("\n")
                    if not raw.strip():
                        continue
                    try:
                        yield from_json_line(raw)
                    except (ValueError, TypeError, json.JSONDecodeError) as exc:
                        if warn:
                            print(
                                f"bus log: skipping malformed line {seg.name}:{lineno} "
                                f"({type(exc).__name__})",
                                file=sys.stderr,
                            )
                        continue
        except OSError as exc:
            if warn:
                print(f"bus log: cannot read segment {seg}: {exc}", file=sys.stderr)
            continue


def iter_thread(thread_id: str, *, warn: bool = True) -> Iterator[Envelope]:
    """Yield every envelope in ``thread_id``, oldest -> newest."""
    for env in iter_messages(warn=warn):
        if env.thread == thread_id:
            yield env
