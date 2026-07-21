"""Per-recipient seen-set for idempotent drain dedup by envelope id (US2).

Bounded-duplicate on the wire, exactly-once at the recipient's eyes (Locked
Decision 3 - no two-phase commit on the file bus). The drain records a message's
``<fno_mail id="...">`` when it CONSUMES it, and drops any later duplicate
delivery carrying that same id. A message with no ``id`` attribute (a pre-redesign
producer) has no key, so it is never deduped -- processed normally.

The seen-set is a small newline-delimited file per recipient under the bus dir,
so it survives a restart and co-isolates with the bus in tests. Growth is bounded
to the most-recent ``_CAP`` ids.
"""
from __future__ import annotations

import re
from pathlib import Path
from typing import Optional

from fno.paths import bus_dir

_CAP = 2000
_ID_RE = re.compile(r'<fno_mail\b[^>]*\bid="([^"]+)"')


def envelope_id(body: str) -> Optional[str]:
    """The ``<fno_mail id="...">`` value in ``body``, or ``None`` when the tag
    carries no id (a pre-redesign message; un-dedupable)."""
    m = _ID_RE.search(body)
    return m.group(1) if m else None


def _seen_path(recipient: str) -> Path:
    safe = recipient.replace("/", "_")
    return bus_dir() / "drain-seen" / f"{safe}.txt"


def already_seen(recipient: str, msg_id: str) -> bool:
    """True if ``msg_id`` was already consumed for ``recipient``. An unreadable
    seen-set reads as empty (degrade to no-dedup, never crash the drain)."""
    try:
        return msg_id in _seen_path(recipient).read_text(encoding="utf-8").split()
    except OSError:
        return False


def mark_seen(recipient: str, msg_id: str) -> None:
    """Record ``msg_id`` as consumed for ``recipient``, bounded to ``_CAP`` ids.
    Best effort: an unwritable seen-set degrades to no-dedup, never raises.

    ponytail: read-modify-write with no lock. Two concurrent drains of one
    recipient could each miss the other's just-written id and double-process a
    duplicate - acceptable at v1 (a drain is normally single per recipient). Add
    a lockfile only if concurrent same-recipient drains become real.
    """
    path = _seen_path(recipient)
    try:
        existing = path.read_text(encoding="utf-8").split() if path.exists() else []
        if msg_id in existing:
            return
        existing.append(msg_id)
        if len(existing) > _CAP:
            existing = existing[-_CAP:]
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("\n".join(existing) + "\n", encoding="utf-8")
    except OSError:
        pass
