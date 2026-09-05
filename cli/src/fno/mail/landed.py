"""Sender-side "did this land" delivery helpers.

The sent-unclaimed predicate remains stat-only and is shared with `fno agents
mail status`. Moved out of `mail/cli.py` (a shrink-only file under the repo's
file budget) so the landed check -- new code proving a hosted row reached its
recipient's transcript -- funds itself by taking the code it touches with it.
"""
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

# Mail text is embedded inside a hook-owned <system-reminder> wrapper, so a
# sender/recipient handle carrying a literal </system-reminder> could break out
# and inject context. Defang the delimiter (open/close, case- + whitespace-
# insensitive) in every interpolated field, mirroring born-with-why-offer-inject.sh.
_REMINDER_TAG = re.compile(r"<\s*(/?)\s*system-reminder\s*>", re.IGNORECASE)


def _defang_reminder(s: str) -> str:
    return _REMINDER_TAG.sub(r"[\1system-reminder]", s)


def _bounded_names(names: list[str], cap: int = 3) -> str:
    """De-dupe (first-seen), defang, then cap at ``cap`` names + ``+K more``."""
    seen: list[str] = []
    for n in names:
        if n not in seen:
            seen.append(n)
    shown = [_defang_reminder(n) for n in seen[:cap]]
    extra = len(seen) - cap
    return ", ".join(shown) + (f", +{extra} more" if extra > 0 else "")


def _parse_ts(ts: str) -> Optional[datetime]:
    """Parse a bus ISO ``ts`` (``...Z`` UTC), or ``None`` if unparseable.

    ``fromisoformat`` is lock-free (unlike ``strptime``, which grabs a global
    locale lock) and pre-3.11-safe once the trailing ``Z`` is normalized -- it
    runs once per sent message on the every-turn hook path, so the lock matters.
    """
    try:
        return datetime.fromisoformat(ts.replace("Z", "+00:00") if ts.endswith("Z") else ts)
    except (ValueError, TypeError, AttributeError):
        return None


def age_minutes(ts: str) -> Optional[int]:
    """Minutes elapsed since bus ISO ``ts``, or ``None`` if unparseable."""
    from datetime import timezone as _tz

    sent_at = _parse_ts(ts)
    if sent_at is None:
        return None
    return int((datetime.now(tz=_tz.utc) - sent_at).total_seconds() // 60)


def _age_exceeds(ts: str, ttl_seconds: int, now: datetime) -> bool:
    """True iff bus ISO ``ts`` is strictly older than TTL.

    Unparseable ts -> False (never flag): degrade to quiet, never to a crash.
    """
    sent_at = _parse_ts(ts)
    if sent_at is None:
        return False
    return (now - sent_at).total_seconds() > ttl_seconds


def _distinct_recipients(msgs: list) -> list[str]:
    """Recipients in first-seen order, one entry each."""
    seen: list[str] = []
    for m in msgs:
        if m.to not in seen:
            seen.append(m.to)
    return seen


def _is_self_send(m: Envelope) -> bool:
    """AC9-ERR: whether ``m``'s recorded recipient session is the sender's own.

    A hosted row's ``to_session`` (change 1) names the session actually
    injected into. When that session is the SAME one that sent the message,
    its transcript trivially carries the id -- the send's own output put it
    there -- so a landed check against it proves nothing about cross-session
    delivery. Refuse the resolution instead of trusting it.
    """
    to_session = (m.meta or {}).get("to_session")
    return bool(to_session) and to_session == m.from_session


def _landed_map(msgs: list[Envelope]) -> dict[str, Optional[bool]]:
    """Per-message landed verdict, one transcript read per distinct recipient
    session rather than one per message (mirrors how ``present_mail_ids``
    batches a single session's own read).

    ``True``/``False`` only for a row carrying usable ``to_session``/
    ``to_harness`` coordinates whose transcript resolved and read. ``None`` --
    never ``False`` -- for a row missing coordinates, a self-send (AC9-ERR),
    or an unreadable transcript (AC3-ERR): a read failure is not evidence of
    absence, so a caller must never turn it into a false negative.
    """
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


def landed_states(
    all_msgs: list[Envelope], msgs: list[Envelope]
) -> dict[str, Optional[bool]]:
    """Landed tri-state for every row in ``msgs``, keyed by id.

    The durable bus proof (``landed_ids``) answers first and needs no grep;
    anything it has not yet recorded falls through to one batched transcript
    read per distinct recipient session (``_landed_map``). Used both by the
    outstanding scan below and by the full ``mail sent`` listing, so a message
    proven landed once never pays for a second grep.
    """
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
    """My sent mail still outstanding past TTL, oldest -> newest.

    Outstanding = not proven landed, AND (for a durable row) still past the
    recipient's consume cursor, AND strictly older than ``ttl_seconds``. A
    hosted row carries no cursor (change 5), so it is judged on landed proof
    and age alone. Reads the bus ONCE (a single ``iter_messages`` snapshot)
    and compares each recipient's cursor position against that snapshot, so
    cost is ``O(bus + recipients)`` not ``O(recipients x bus)``.

    Returns the envelopes rather than a count because for its whole life this
    computed exactly the rows a sender needs to act -- id, recipient, age --
    and threw all but the tally away. Callers that only want the tally take
    ``len()``.

    A message older than ``config.inbox.landed_abandon_ttl`` is dropped
    outright (AC8-EDGE): outstanding at 30 minutes is information, at six
    hours it is noise, and it is never greped again either way.
    """
    from datetime import datetime as _dt
    from datetime import timezone as _tz

    from fno.bus.cursor import read_cursor
    from fno.config import load_settings

    now = _dt.now(tz=_tz.utc)
    all_msgs = list(iter_messages())
    # A withdrawn message stops being outstanding the moment it is retracted;
    # this reader takes `iter_messages` directly and so is NOT covered by the
    # filter in `scan_unread`. Without this line the nag survives its own
    # withdrawal.
    retracted = withdrawn_ids(all_msgs)
    already_landed = landed_ids(all_msgs)
    abandon_ttl = load_settings().inbox.landed_abandon_ttl
    sent = [
        m for m in all_msgs
        if m.kind == "send"
        and m.from_ == handle
        and m.id not in retracted
        and m.id not in already_landed
        and m.delivery != TYPED_DELIVERY
        and not _age_exceeds(m.ts, abandon_ttl, now)
    ]
    if not sent:
        return []
    pos = {m.id: i for i, m in enumerate(all_msgs)}
    # Per durable recipient, its consume-cursor position in the single
    # snapshot. A hosted row has no cursor to compare against (it is judged
    # on landed proof alone), so it is excluded from this lookup entirely --
    # comparing it against a cursor advanced by an unrelated LATER durable
    # message would read a never-consumed hosted send as claimed.
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
            # First sighting: persist the proof so it survives a transcript
            # rotation or a dead recipient session (AC5-EDGE), and so every
            # later scan skips the grep entirely via `already_landed` above.
            record_landed(msg_id=m.id, sender=m.from_, recipient=m.to)
            continue
        out.append(m)
    return out


def nag_line(unclaimed: list) -> Optional[str]:
    """The sender turn-boundary line for outstanding mail, or ``None`` when
    everything has landed (AC6-HP).

    Names the keystroke because no receipt can perform it: a hosted send that
    never reaches its recipient's turn only resolves when a human interrupts
    the loop it is stuck in.
    """
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
