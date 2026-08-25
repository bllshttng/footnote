"""Rolling word budget for one canonical sender-recipient pair.

Rule 7 caps one message at 80 masked words. The rolling policy caps each
canonical pair at 80 words over 10 minutes. Any inbound message from the
recipient back to the sender resets the running total.

Time catches an unanswered burst. The inbound reset is what separates evasion
from a real conversation, and it is proven by an addressed bus envelope from
recipient to sender, never by a liveness probe or by receipt text.

The reservation is taken BEFORE any outward delivery and released only on a
proven non-delivery. A crash after the body has left leaves the reservation
charged: a conservative overcharge expires in 10 minutes, while the opposite
error would make a delivered message free.

Counting lives in :func:`fno.style.word_count`. This module never counts.
"""
from __future__ import annotations

import hashlib
import json
import os
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

# Both values are fixed policy, not configuration. A per-project override would
# let the noisiest fleet raise its own cap.
CAP = 80
WINDOW_SECONDS = 600


class BudgetRefused(Exception):
    """The projected pair total exceeds the cap inside the window."""

    def __init__(
        self,
        *,
        pair: str,
        running: int,
        current: int,
    ) -> None:
        self.pair = pair
        self.running = running
        self.current = current
        self.projected = running + current
        self.cap = CAP
        self.window_seconds = WINDOW_SECONDS
        super().__init__(self.marker())

    def marker(self) -> str:
        """The receipt line. Every number a sender needs to see is named."""
        return (
            f"running={self.running} current={self.current} "
            f"projected={self.projected} cap={self.cap} "
            f"window={self.window_seconds // 60}m"
        )


class BudgetUnavailable(Exception):
    """The pair ledger could not be read or locked, so the send fails closed.

    A malformed ACTIVE ledger refuses rather than resetting to zero. Reading a
    damaged counter as empty is how a cap becomes optional under corruption.
    """

    def __init__(self, pair: str, detail: str, path: Path) -> None:
        self.pair = pair
        super().__init__(
            f"word budget unavailable for pair {pair}: {detail}. "
            f"Recovery: inspect {path}, then remove it to start a new window."
        )


@dataclass(frozen=True)
class Reservation:
    """One charged message. Hold it until delivery is proven one way or other."""

    pair: str
    entry_id: str
    running_before: int
    words: int
    reset_by: Optional[str] = None


def pair_label(sender: str, recipient: str) -> str:
    """The canonical pair, resolved. Never the caller's alias."""
    return f"{sender} -> {recipient}"


def _ledger_path(pair: str) -> Path:
    from fno import paths

    digest = hashlib.sha256(pair.encode("utf-8")).hexdigest()[:16]
    return paths.bus_dir() / "word-budget" / f"{digest}.json"


def _acquire(path: Path, pair: str):
    from fno.inbox.store import _acquire_lock

    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        return _acquire_lock(path, timeout=10.0)
    except TimeoutError as exc:
        raise BudgetUnavailable(pair, f"lock contended: {exc}", path) from exc


def _release(lock_dir) -> None:
    from fno.inbox.store import _release_lock

    _release_lock(lock_dir)


def _load(path: Path, pair: str) -> list[dict]:
    if not path.exists():
        return []
    try:
        obj = json.loads(path.read_text())
    except (OSError, ValueError) as exc:
        raise BudgetUnavailable(pair, f"unreadable ledger: {exc}", path) from exc
    entries = obj.get("entries") if isinstance(obj, dict) else None
    if not isinstance(entries, list):
        raise BudgetUnavailable(pair, "ledger has no entries list", path)
    out: list[dict] = []
    for e in entries:
        if isinstance(e, dict) and isinstance(e.get("ts"), (int, float)) and isinstance(e.get("words"), int):
            out.append(e)
        else:
            raise BudgetUnavailable(pair, "ledger holds a malformed entry", path)
    return out


def _store(path: Path, pair: str, entries: list[dict]) -> None:
    if not entries:
        # Opportunistic: a pair that went quiet leaves no file behind.
        try:
            path.unlink()
        except OSError:
            pass
        return
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps({"pair": pair, "entries": entries}, indent=None))
    os.replace(tmp, path)


def last_inbound(sender: str, recipient: str, *, since: float) -> Optional[tuple[str, float]]:
    """Newest bus envelope addressed from ``recipient`` to ``sender`` since ``since``.

    Reads the canonical bus, which records the hosted lane as well as the durable
    one, so a reply delivered live still counts as a reply. Before that recording
    landed the hosted lane was invisible here and the reset would have fired only
    for durable replies.
    """
    from fno.bus.log import iter_messages
    from fno.mail.kinds import is_authored_mail_kind

    if sender == recipient:
        return None
    newest: Optional[tuple[str, float]] = None
    for env in iter_messages(warn=False):
        if (
            not is_authored_mail_kind(env.kind)
            or env.from_ != recipient
            or env.to != sender
        ):
            continue
        ts = _parse_ts(env.ts)
        if ts is None or ts < since:
            continue
        if newest is None or ts >= newest[1]:
            newest = (env.id, ts)
    return newest


def _parse_ts(raw: str) -> Optional[float]:
    from datetime import datetime, timezone

    if not raw:
        return None
    try:
        text = raw.replace("Z", "+00:00")
        return datetime.fromisoformat(text).replace(tzinfo=timezone.utc).timestamp()
    except ValueError:
        return None


def reserve(
    *,
    sender: str,
    recipient: str,
    words: int,
    msg_id: str,
    enforce: bool = True,
    sender_key: Optional[str] = None,
    recipient_key: Optional[str] = None,
) -> Reservation:
    """Charge ``words`` to the pair, refusing when the projection breaks the cap.

    ``enforce=False`` is the style-exception path: the send is permitted, but it
    is still charged. An exception is authority to exceed the cap once, never a
    licence to spend the window unrecorded. A REFUSED attempt is never charged.

    ``sender_key`` / ``recipient_key`` rekey the LEDGER while the display pair
    and the inbound-reset lookup keep the handles callers address by. Eight-hex
    handles collide for time-ordered codex ids (two siblings spawned inside one
    ~65s bucket share a prefix), so a caller that holds the occupant's full
    session id keys the ledger on it and the siblings charge separately. The
    inbound reset must keep matching bus envelopes, which carry handles.
    """
    pair = pair_label(sender_key or sender, recipient_key or recipient)
    path = _ledger_path(pair)
    now = time.time()
    floor = now - WINDOW_SECONDS

    lock = _acquire(path, pair)
    try:
        entries = [e for e in _load(path, pair) if e["ts"] >= floor]
        reset_by = None
        inbound = last_inbound(sender, recipient, since=floor)
        inbound_id = None
        if inbound is not None:
            reset_id, reset_ts = inbound
            inbound_id = reset_id
            kept = [
                entry
                for entry in entries
                if (
                    entry.get("inbound_id") == reset_id
                    if "inbound_id" in entry
                    else entry["ts"] > reset_ts
                )
            ]
            if len(kept) != len(entries):
                reset_by = reset_id
            entries = kept
        running = sum(e["words"] for e in entries)
        if enforce and running + words > CAP:
            # Refused attempts never consume budget: the pruned ledger is still
            # written back so an expired window is not re-read on the next send.
            _store(path, pair, entries)
            raise BudgetRefused(pair=pair, running=running, current=words)
        entries.append(
            {
                "id": msg_id,
                "ts": now,
                "words": words,
                "inbound_id": inbound_id,
            }
        )
        _store(path, pair, entries)
        return Reservation(
            pair=pair,
            entry_id=msg_id,
            running_before=running,
            words=words,
            reset_by=reset_by,
        )
    finally:
        _release(lock)


def release(reservation: Reservation) -> None:
    """Give back a reservation whose message provably never left.

    Only a refusal or a confirmed failure releases. A send that reached the
    recipient stays charged until the window expires or an inbound reply resets
    it. Best-effort: a failed release overcharges for at most 10 minutes, which
    is the safe direction.
    """
    path = _ledger_path(reservation.pair)
    try:
        lock = _acquire(path, reservation.pair)
    except BudgetUnavailable:
        return
    try:
        entries = [e for e in _load(path, reservation.pair) if e.get("id") != reservation.entry_id]
        _store(path, reservation.pair, entries)
    except BudgetUnavailable:
        return
    finally:
        _release(lock)
