"""Sender-side "did this land" helpers: sent-unclaimed predicate (shared with
`fno agents mail status`) and the landed check. Moved out of shrink-only `mail/cli.py`."""
from __future__ import annotations

import re
from datetime import datetime
from typing import Optional

from fno.bus.log import (
    Envelope,
    HOSTED_DELIVERY,
    TYPED_DELIVERY,
    iter_messages,
    landed_ids,
    record_landed,
    withdrawn_ids,
)
from fno.mail.reply_resolve import mail_ids_in_transcript

# Defang a literal </system-reminder> that could break out of the hook wrapper.
_REMINDER_TAG = re.compile(r"<\s*(/?)\s*system-reminder\s*>", re.IGNORECASE)


def _defang_reminder(s: str) -> str:
    return _REMINDER_TAG.sub(r"[\1system-reminder]", s)


def _bounded_names(names: list[str], cap: int = 3) -> str:
    """De-dupe (first-seen), defang, then cap at ``cap`` names + ``+K more``."""
    seen = list(dict.fromkeys(names))
    shown = [_defang_reminder(n) for n in seen[:cap]]
    extra = len(seen) - cap
    return ", ".join(shown) + (f", +{extra} more" if extra > 0 else "")


def _parse_ts(ts: str) -> Optional[datetime]:
    """Parse a bus ISO ``ts``, or ``None`` if unparseable (never raises)."""
    try:
        return datetime.fromisoformat(ts.replace("Z", "+00:00") if ts.endswith("Z") else ts)
    except (ValueError, TypeError, AttributeError):
        return None


def age_minutes(ts: str) -> Optional[int]:
    """Minutes elapsed since bus ISO ``ts``, or ``None`` if unparseable."""
    from datetime import timezone as _tz

    sent_at = _parse_ts(ts)
    return None if sent_at is None else int((datetime.now(tz=_tz.utc) - sent_at).total_seconds() // 60)


def _age_exceeds(ts: str, ttl_seconds: int, now: datetime) -> bool:
    """True iff ``ts`` is strictly older than TTL. Unparseable -> False."""
    sent_at = _parse_ts(ts)
    return sent_at is not None and (now - sent_at).total_seconds() > ttl_seconds


def _distinct_recipients(msgs: list) -> list[str]:
    """Recipients in first-seen order, one entry each."""
    return list(dict.fromkeys(m.to for m in msgs))


def _is_self_send(m: Envelope) -> bool:
    """Recipient IS the sender: its own transcript trivially carries the id."""
    to_session = (m.meta or {}).get("to_session")
    return bool(to_session) and to_session == m.from_session


def _landed_map(msgs: list[Envelope]) -> dict[str, Optional[bool]]:
    """Per-message landed verdict, one transcript read per recipient session.
    ``None`` when coordinates are missing, self-send, or unreadable."""
    out: dict[str, Optional[bool]] = {}
    by_store: dict[tuple[str, str], list[Envelope]] = {}
    for m in msgs:
        if _is_self_send(m):
            out[m.id] = None
            continue
        to_session = (m.meta or {}).get("to_session")
        to_harness = (m.meta or {}).get("to_harness")
        if not to_session or not to_harness:
            out[m.id] = None
            continue
        by_store.setdefault((to_harness, to_session), []).append(m)
    for (harness, session_id), rows in by_store.items():
        ids = mail_ids_in_transcript(harness, session_id)
        for m in rows:
            out[m.id] = None if ids is None else m.id in ids
    return out


def landed_states(all_msgs: list[Envelope], msgs: list[Envelope]) -> dict[str, Optional[bool]]:
    """Landed tri-state for ``msgs``: durable proof first, then a transcript read."""
    already = landed_ids(all_msgs)
    out: dict[str, Optional[bool]] = {}
    to_check = []
    for m in msgs:
        if m.id in already:
            out[m.id] = True
        else:
            to_check.append(m)
    out.update(_landed_map(to_check))
    return out


def _sent_unclaimed(handle: str, ttl_seconds: int) -> list:
    """Sent mail outstanding past TTL: not landed, past its cursor (durable
    only), older than ``ttl_seconds``. Past ``landed_abandon_ttl`` drops out."""
    from datetime import datetime as _dt, timezone as _tz

    from fno.bus.cursor import read_cursor
    from fno.config import load_settings

    now = _dt.now(tz=_tz.utc)
    all_msgs = list(iter_messages())
    retracted = withdrawn_ids(all_msgs)
    already_landed = landed_ids(all_msgs)
    abandon_ttl = load_settings().inbox.landed_abandon_ttl
    sent = [
        m for m in all_msgs
        if m.kind == "send" and m.from_ == handle and m.id not in retracted
        and m.id not in already_landed and m.delivery != TYPED_DELIVERY
        and not _age_exceeds(m.ts, abandon_ttl, now)
    ]
    if not sent:
        return []
    pos = {m.id: i for i, m in enumerate(all_msgs)}
    # A hosted row has no cursor; comparing it to one advanced by an unrelated
    # later durable message would misread a never-consumed send as claimed.
    cursor_pos: dict[str, int] = {}
    for r in {m.to for m in sent if m.delivery != HOSTED_DELIVERY}:
        try:
            cid = read_cursor(r)
        except (ValueError, OSError):
            cursor_pos[r] = len(all_msgs)
            continue
        cursor_pos[r] = pos.get(cid, -1) if cid else -1
    candidates = []
    for m in sent:
        if m.delivery != HOSTED_DELIVERY and pos[m.id] <= cursor_pos.get(m.to, len(all_msgs)):
            continue  # durable: claimed via cursor
        if not _age_exceeds(m.ts, ttl_seconds, now):  # still fresh (strict >)
            continue
        candidates.append(m)
    if not candidates:
        return []
    states = landed_states(all_msgs, candidates)
    out = []
    for m in candidates:
        if states.get(m.id) is True:
            # Persists past a transcript rotation; later scans skip the grep.
            record_landed(msg_id=m.id, sender=m.from_, recipient=m.to)
            continue
        out.append(m)
    return out


def nag_line(unclaimed: list) -> Optional[str]:
    """Turn-boundary nag line for outstanding mail, or ``None`` when all landed."""
    if not unclaimed:
        return None
    who = _bounded_names(_distinct_recipients(unclaimed))
    age_min = age_minutes(unclaimed[0].ts) or 0
    n = len(unclaimed)
    noun = "message" if n == 1 else "messages"
    return (
        f"{n} {noun} to {who} handed {age_min}m ago, not in transcript - "
        "they are mid-loop, ESC to steer"
    )
