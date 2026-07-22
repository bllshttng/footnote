"""Per-recipient seen-set for idempotent drain dedup by envelope id (US2).

Bounded-duplicate on the wire, exactly-once at the recipient's eyes (Locked
Decision 3 - no two-phase commit on the file bus). The drain records a message's
``<fno_mail id="...">`` when it CONSUMES it, and drops any later duplicate
delivery carrying that same envelope. A message with no ``id`` attribute (a
pre-redesign producer) has no key, so it is never deduped - processed normally.

The dedup key is the sha256 of the whole paired ``<fno_mail>...</fno_mail>``
block, NOT the 24-bit ``generate_msg_id`` alone: a bounded-duplicate is a
byte-identical envelope (same key), while two DIFFERENT messages that happen to
collide on the 24-bit id have different from/to/text (different key), so a
collision can never silently drop a legitimate message.

The seen-set is a small newline-delimited file per recipient under the bus dir,
so it survives a restart and co-isolates with the bus in tests. Growth is bounded
to the most-recent ``_CAP`` keys.
"""
from __future__ import annotations

import hashlib
import re
from pathlib import Path
from typing import Optional

from fno.paths import bus_dir

_CAP = 2000
# The whole paired envelope block (DOTALL: body spans newlines).
_ENVELOPE_RE = re.compile(r"<fno_mail\b[^>]*>.*?</fno_mail>", re.DOTALL)
# The open tag carries an id attribute (the "is this dedupable" gate).
_HAS_ID_RE = re.compile(r'<fno_mail\b[^>]*\bid="[^"]+"')


def dedup_key(body: str) -> Optional[str]:
    """A collision-resistant dedup key for ``body``: the sha256 of its paired
    ``<fno_mail>...</fno_mail>`` block, but only when that block carries an ``id``
    attribute. ``None`` for a block with no id (pre-redesign; un-dedupable) or no
    envelope at all - the caller then processes the message normally."""
    m = _ENVELOPE_RE.search(body)
    if not m or not _HAS_ID_RE.match(m.group(0)):
        return None
    return hashlib.sha256(m.group(0).encode("utf-8")).hexdigest()[:32]


def _seen_path(recipient: str) -> Path:
    safe = recipient.replace("/", "_")
    return bus_dir() / "drain-seen" / f"{safe}.txt"


def already_seen(recipient: str, key: str) -> bool:
    """True if ``key`` was already consumed for ``recipient``. An unreadable
    seen-set reads as empty (degrade to no-dedup, never crash the drain)."""
    try:
        return key in _seen_path(recipient).read_text(encoding="utf-8").split()
    except OSError:
        return False


def mark_seen(recipient: str, key: str) -> None:
    """Record ``key`` as consumed for ``recipient``, bounded to ``_CAP`` keys.
    Best effort: an unwritable seen-set degrades to no-dedup, never raises.

    ponytail: read-modify-write with no lock. Two concurrent drains of one
    recipient could each miss the other's just-written key and double-process a
    duplicate - acceptable at v1 (a drain is normally single per recipient). Add
    a lockfile only if concurrent same-recipient drains become real.
    """
    path = _seen_path(recipient)
    try:
        existing = path.read_text(encoding="utf-8").split() if path.exists() else []
        if key in existing:
            return
        existing.append(key)
        if len(existing) > _CAP:
            existing = existing[-_CAP:]
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("\n".join(existing) + "\n", encoding="utf-8")
    except OSError:
        pass
