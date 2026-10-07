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
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterator, Optional

from fno.rust_binary import VerbUnavailable, chats_verb
from fno.time_budget import validate_timeout_budget


ENVELOPE_VERSION = 1
HOSTED_DELIVERY = "hosted"
#: `fno agents mail send --force`: the body was TYPED into a pane as keystrokes. Kept
#: distinct from `hosted` on purpose (node). Bytes written to a PTY is
#: not delivery and is certainly not action -- a full payload can arrive, render,
#: and be discarded while the return selects a prompt's default. The row says
#: what happened and no more, and names the pane a reader can go read.
TYPED_DELIVERY = "typed"

#: Audit-only: outage-era traffic backfilled with full provenance, never re-delivered.
CROSS_SESSION_DELIVERY = "cross-session"

# Size-triggered rotation now lives in the Rust bus-append door
# (fno-agents announce::append_line), which reads the same
# FNO_BUS_MAX_BYTES / FNO_BUS_RETAIN envs. A malformed override degrades
# to the default rather than raising.
_LOCK_TIMEOUT_SECONDS = 5.0
_LOCK_POLL_SECONDS = 0.05


# ---------------------------------------------------------------------------
# Envelope (versioned)
# ---------------------------------------------------------------------------

@dataclass
class Envelope:
    """One line in the bus log. ``from_`` serializes to the canonical ``from`` key.

    ``from`` and ``to`` are the addresses (registry names, or a project name in
    ``--to-project`` durable mode). ``from_harness``/``to_harness`` are
    metadata-only tags for transport selection and audit, never for addressing
    (they hold a harness name, not a provider; the v2 reshape renamed them from
    ``provider_from``/``provider_to``).
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
    from_harness: Optional[str] = None
    to_harness: Optional[str] = None
    request_id: Optional[str] = None
    in_reply_to: Optional[str] = None
    delivery: Optional[str] = None
    meta: dict = field(default_factory=dict)
    # Addressed-delivery enrichment (Group 1, / cv-d54ddd45). All
    # optional and omitted from the line when unset, so pre-existing lines are
    # byte-unchanged and old lines still parse (LD11 additive read).
    #  - from_session: the sender's session id, used to exclude the sender on a
    #    to_kind=project broadcast read (you never drain your own broadcast).
    #  - from_model:   the sender's model, surfaced in the render/projection.
    #  - to_kind:      addressing discriminator: "name" | "session" | "project".
    from_session: Optional[str] = None
    from_model: Optional[str] = None
    to_kind: Optional[str] = None
    # Send-time masked prose count. Additive: a row written before this
    # field existed reads back as None and never acquires a fabricated count.
    # The send lane supplies it so the row and Rule 7 both carry the count of
    # the SAME string -- the raw body, not the wire wrapper.
    word_count: Optional[int] = None
    # Provenance axis (ruling d-b328d8c4), classified at write time by
    # classify_origin and surfaced by the drain render as an origin label.
    # Additive: a row written before this field existed reads back through the
    # legacy meta fallback.
    origin: Optional[str] = None
    # The sender's --subject. Additive: a row written before this
    # field existed reads back as None and the envelope header derives its
    # third field from the body as before.
    subject: Optional[str] = None

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
        from_harness: Optional[str] = None,
        to_harness: Optional[str] = None,
        request_id: Optional[str] = None,
        in_reply_to: Optional[str] = None,
        delivery: Optional[str] = None,
        meta: Optional[dict] = None,
        from_session: Optional[str] = None,
        from_model: Optional[str] = None,
        to_kind: Optional[str] = None,
        word_count: Optional[int] = None,
        origin: Optional[str] = None,
        subject: Optional[str] = None,
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
            from_harness=from_harness,
            to_harness=to_harness,
            request_id=request_id,
            in_reply_to=in_reply_to,
            delivery=delivery,
            meta=dict(meta or {}),
            from_session=from_session,
            from_model=from_model,
            to_kind=to_kind,
            word_count=word_count,
            origin=origin,
            subject=subject,
        )


def new_msg_id() -> str:
    """A 'fmail-XXXXXXXXXXXX' id (12 hex); Rust twin announce.rs::new_msg_id."""
    return "fmail-" + secrets.token_hex(6)


def _now_iso() -> str:
    return datetime.now(tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# Canonical key order. Always-present keys first, then optional tags, then body
# last (bodies can be large; keeping them last keeps the line head scannable).


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
    if env.from_harness:
        obj["from_harness"] = env.from_harness
    if env.to_harness:
        obj["to_harness"] = env.to_harness
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
    if env.origin:
        obj["origin"] = env.origin
    if env.subject:
        obj["subject"] = env.subject
    # `is not None`, not truthiness: a genuine zero-word body (a pasted log
    # masks to nothing) must serialize as 0, not vanish and read back as legacy.
    if env.word_count is not None:
        obj["word_count"] = env.word_count
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
        # v2 rename: stored rows carry the old provider_* key, so the read
        # accepts both, following the legacy fallback `origin` uses below.
        from_harness=obj.get("from_harness") or obj.get("provider_from"),
        to_harness=obj.get("to_harness") or obj.get("provider_to"),
        request_id=obj.get("request_id"),
        in_reply_to=obj.get("in_reply_to"),
        delivery=obj.get("delivery"),
        meta=_meta if isinstance((_meta := obj.get("meta")), dict) else {},
        from_session=obj.get("from_session"),
        from_model=obj.get("from_model"),
        to_kind=obj.get("to_kind"),
        word_count=_wc if isinstance((_wc := obj.get("word_count")), int) else None,
        # Legacy rows carried origin inside meta only; prefer the field, fall
        # back to meta so pre-existing lines still render their provenance.
        origin=obj.get("origin")
        or (_meta.get("origin") if isinstance(_meta, dict) else None),
        subject=obj.get("subject"),
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

class BusLockTimeout(TimeoutError):
    """The canonical bus sidecar stayed contended past its write budget."""

    def __init__(self, lock_path: Path, timeout_seconds: float):
        self.lock_path = lock_path
        self.timeout_seconds = timeout_seconds
        super().__init__(
            f"bus lock timeout after {timeout_seconds:g}s at {lock_path}; "
            "no durable envelope was written"
        )


class _Flock:
    """Context manager holding an exclusive flock on the sidecar lockfile.

    The lockfile is separate from the log itself so the lock survives a
    rotation rename of ``messages.jsonl``.
    """

    def __init__(
        self,
        lock_path: Path,
        *,
        timeout_seconds: Optional[float] = None,
        poll_seconds: Optional[float] = None,
    ):
        self._lock_path = lock_path
        self._fd: Optional[int] = None
        self._timeout_seconds = (
            _LOCK_TIMEOUT_SECONDS if timeout_seconds is None else timeout_seconds
        )
        self._poll_seconds = _LOCK_POLL_SECONDS if poll_seconds is None else poll_seconds

    def __enter__(self) -> "_Flock":
        validate_timeout_budget(
            self._timeout_seconds,
            label="bus lock",
            poll=self._poll_seconds,
        )
        self._lock_path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(str(self._lock_path), os.O_CREAT | os.O_RDWR, 0o644)
        try:
            deadline = time.monotonic() + self._timeout_seconds
            while True:
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise BusLockTimeout(
                            self._lock_path,
                            self._timeout_seconds,
                        )
                    time.sleep(min(self._poll_seconds, remaining))
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


def append(env: Envelope) -> None:
    """Append one envelope through the Rust bus-append door.

    The door owns the sidecar lock, the size rotation and the 0o600 mode,
    and it records the chats row itself (the record seam and the receipt).
    Lock-free readers may transiently miss the just-renamed live->.1
    segment during a rotation; that window is covered by the cursor
    fallback, so a message is at most delayed by one drain cycle, never
    dropped. Python keeps the serializer and the one call (AC20-HP). A
    contended door lock re-raises as :class:`BusLockTimeout`, the
    exception contract the send path already catches.
    """
    try:
        # The live path rides argv so the door writes wherever THIS
        # resolver points: config.paths.bus_dir and both env legs stay
        # Python's single source (the door has no settings reader).
        chats_verb(["bus-append", str(bus_log_path())], json.loads(to_json_line(env)))
    except VerbUnavailable as exc:
        if "bus lock timeout" in str(exc):
            raw = os.environ.get("FNO_BUS_LOCK_TIMEOUT_SECS")
            try:
                timeout = float(raw) if raw else _LOCK_TIMEOUT_SECONDS
            except ValueError:
                timeout = _LOCK_TIMEOUT_SECONDS
            raise BusLockTimeout(_lock_path(), timeout) from exc
        raise


#: The tombstone kind. A withdrawal cannot delete a line (the log is
#: append-only) and must not advance the RECIPIENT's cursor, which is a
#: last-seen position rather than a per-message flag - moving it would swallow
#: every other unread message addressed to them. So a withdrawal is one more
#: appended envelope naming the message it retracts, and readers skip the pair.
WITHDRAW_KIND = "withdraw"

#: Durable proof one id reached its recipient's transcript. Never deliverable.
LANDED_KIND = "landed"

#: Rows that are ledger traffic about other messages, never inbox content.
CONTROL_KINDS = frozenset({WITHDRAW_KIND, LANDED_KIND})


def is_deliverable(env: Envelope) -> bool:
    """Whether a bus envelope is pending recipient delivery.

    A control row (a withdrawal tombstone, a landed receipt) is bookkeeping
    about another message and is never inbox content, whatever its `to` says.

    Missing delivery metadata is the legacy durable shape. A hosted row records
    a delivery that already succeeded and exists only for sender/operator audit.
    A typed row records bytes already written into the recipient's pane, so it
    is audit-only too -- draining it would hand the recipient a second copy of
    text already sitting at its prompt. A cross-session row is historical
    traffic the archive already holds.
    """
    if getattr(env, "kind", None) in CONTROL_KINDS:
        return False
    delivery = getattr(env, "delivery", None)
    return delivery not in (HOSTED_DELIVERY, TYPED_DELIVERY, CROSS_SESSION_DELIVERY)


def record_hosted_delivery(
    *,
    msg_id: str,
    sender: str,
    recipient: str,
    body: str,
    thread: Optional[str] = None,
    from_harness: Optional[str] = None,
    to_harness: Optional[str] = None,
    request_id: Optional[str] = None,
    in_reply_to: Optional[str] = None,
    from_session: Optional[str] = None,
    from_model: Optional[str] = None,
    to_kind: Optional[str] = None,
    word_count: Optional[int] = None,
    to_session: Optional[str] = None,
    subject: Optional[str] = None,
) -> Envelope:
    """Append one audit-only record after confirmed hosted delivery. ``to_session``/
    ``to_harness`` name the session actually injected into, for the landed check
    (which reads the ``to_harness`` meta copy)."""
    meta = {k: v for k, v in (("to_session", to_session), ("to_harness", to_harness)) if v}
    env = Envelope.new(
        id=msg_id,
        thread=thread or msg_id,
        from_=sender,
        to=recipient,
        kind="send",
        body=body,
        from_harness=from_harness,
        to_harness=to_harness,
        request_id=request_id,
        in_reply_to=in_reply_to,
        delivery=HOSTED_DELIVERY,
        from_session=from_session,
        from_model=from_model,
        to_kind=to_kind,
        word_count=word_count,
        meta=meta or None,
        subject=subject,
    )
    append(env)
    return env


def record_typed_delivery(
    *,
    msg_id: str,
    sender: str,
    recipient: str,
    body: str,
    pane_id: str,
    mux_session: Optional[str] = None,
    thread: Optional[str] = None,
    from_harness: Optional[str] = None,
    to_harness: Optional[str] = None,
    in_reply_to: Optional[str] = None,
    from_session: Optional[str] = None,
    from_model: Optional[str] = None,
    to_kind: Optional[str] = None,
    word_count: Optional[int] = None,
    subject: Optional[str] = None,
) -> Envelope:
    """Append one audit-only record after a ``--force`` pane send typed the body.

    The mapping from ``msg_id`` to ``pane_id`` is the whole point (node).
    A message delivered by keystroke used to be invisible to every mail surface:
    the recipient saw text with no id and the sender's outbox had no row. With
    the mapping, ``fno agents mail sent`` shows the message and names the transport, and
    a payload that lands in a pane and is never consumed becomes traceable to a
    pane a reader can go read.
    """
    env = Envelope.new(
        id=msg_id,
        thread=thread or msg_id,
        from_=sender,
        to=recipient,
        kind="send",
        body=body,
        from_harness=from_harness,
        to_harness=to_harness,
        in_reply_to=in_reply_to,
        delivery=TYPED_DELIVERY,
        # The reply address, on the row a reader reaches for first. `cmd_reply`
        # consults the bus before any transcript, so a forced message whose row
        # carried only the head-8 handle refused as ambiguous - on the one
        # transport with no live confirmation to fall back to.
        from_session=from_session,
        from_model=from_model,
        to_kind=to_kind,
        word_count=word_count,
        subject=subject,
        meta={
            "transport": "pane",
            "pane_id": str(pane_id),
            **({"mux_session": mux_session} if mux_session else {}),
        },
    )
    append(env)
    return env


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


def withdrawn_ids(msgs: list[Envelope]) -> set[str]:
    """Ids retracted by a tombstone, plus the tombstone ids themselves.

    Both halves are returned because a reader that hid the target but rendered
    the tombstone would deliver a bare "this was withdrawn" line to a recipient
    who never saw the original.

    A tombstone counts only when it retracts a message the same sender sent to
    the same address. That check lives here, at the read side, rather than only
    at the write verb: the write verb is one reachable path, and a hand-appended
    or replayed line must not be able to retract someone else's mail.
    """
    by_id = {m.id: m for m in msgs}
    out: set[str] = set()
    for m in msgs:
        if m.kind != WITHDRAW_KIND:
            continue
        # A tombstone is control traffic and is never itself deliverable, so it
        # is suppressed BEFORE the sender check rather than after. An invalid
        # one that fell through would arrive in the recipient's inbox as a bare
        # "withdrawn: msg-xxxx" line about a message it had no power to retract.
        out.add(m.id)
        target_id = (m.meta or {}).get("withdraws")
        target = by_id.get(target_id) if isinstance(target_id, str) else None
        if target is None or target.from_ != m.from_ or target.to != m.to:
            continue
        out.add(target.id)
    return out


def record_landed(*, msg_id: str, sender: str, recipient: str) -> Envelope:
    """Append one control row proving ``msg_id`` reached ``recipient``'s transcript.

    The row is control traffic that no inbox renders; it only proves delivery.
    """
    env = Envelope.new(
        from_=sender,
        to=recipient,
        kind=LANDED_KIND,
        body="",
        meta={"landed": msg_id},
    )
    append(env)
    return env


def landed_ids(msgs: list[Envelope]) -> set[str]:
    """Ids proven landed by a same-sender/recipient ``record_landed`` row (mirrors
    ``withdrawn_ids``). The control row's own id is never added, and the two
    facts stay separate: the acknowledged message stays visible to its
    recipient, and the acknowledging row is control traffic that no inbox renders."""
    by_id = {m.id: m for m in msgs}
    out: set[str] = set()
    for m in msgs:
        if m.kind != LANDED_KIND:
            continue
        target_id = (m.meta or {}).get("landed")
        target = by_id.get(target_id) if isinstance(target_id, str) else None
        if target is None or target.from_ != m.from_ or target.to != m.to:
            continue
        out.add(target.id)
    return out


def iter_thread(thread_id: str, *, warn: bool = True) -> Iterator[Envelope]:
    """Yield every envelope in ``thread_id``, oldest -> newest."""
    for env in iter_messages(warn=warn):
        if env.thread == thread_id:
            yield env
