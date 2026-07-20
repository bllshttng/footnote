"""Per-agent read cursors over the bus log.

The log is never mutated; read/unread is a per-consumer cursor file
(``<bus_dir>/cursors/<name>.json``) keyed by the last-seen message-id, never a
raw byte offset, so a rotation cannot silently reset or skip a read position
(locked decision 7). "My inbox" is a cursor-bounded view over the one global
log, filtered to ``to == me`` - not a physical per-recipient file.

Failure posture is fail-open toward never losing unprocessed mail:
  - absent cursor   -> scan from the start of retained segments (a never-seen
                       peer still receives durable mail), not "from now".
  - corrupt cursor  -> treated as absent (rescan), with a warning.
  - cursor id gone  -> (rotated out) rescan retained segments; worst case the
                       consumer re-sees old messages, deduped by sink idempotency.
"""
from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path
from typing import Iterable, Optional

from fno.bus.log import Envelope, iter_messages


def _cursors_dir() -> Path:
    from fno import paths
    return paths.bus_dir() / "cursors"


def _safe_name(name: str) -> str:
    """Reject path-traversal in a consumer name before composing a cursor path."""
    if not name or "/" in name or "\\" in name or ".." in name:
        raise ValueError(f"invalid cursor name: {name!r}")
    return name


def cursor_path(name: str) -> Path:
    """Path to a consumer's cursor file (``<bus_dir>/cursors/<name>.json``)."""
    return _cursors_dir() / f"{_safe_name(name)}.json"


def read_cursor(name: str) -> Optional[str]:
    """Return the last-seen message-id for ``name``, or None if unset/corrupt."""
    p = cursor_path(name)
    if not p.exists():
        return None
    try:
        obj = json.loads(p.read_text(encoding="utf-8"))
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        print(
            f"bus cursor: ignoring corrupt cursor {p.name} ({type(exc).__name__}); "
            f"rescanning retained segments",
            file=sys.stderr,
        )
        return None
    if isinstance(obj, dict) and obj.get("last_seen_id"):
        return str(obj["last_seen_id"])
    return None


def write_cursor(name: str, msg_id: str) -> None:
    """Atomically write ``name``'s cursor to ``msg_id`` (sibling temp + replace)."""
    from datetime import datetime, timezone

    p = cursor_path(name)
    p.parent.mkdir(parents=True, exist_ok=True)
    payload = json.dumps(
        {"last_seen_id": msg_id, "ts": datetime.now(tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")},
        ensure_ascii=False,
    )
    fd, tmp_str = tempfile.mkstemp(dir=str(p.parent), prefix=f".{p.name}.tmp.", suffix=".part")
    tmp = Path(tmp_str)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(payload)
        os.replace(str(tmp), str(p))
    except Exception:
        try:
            tmp.unlink()
        except OSError:
            pass
        raise


# advance_cursor is the verb the drain/ack paths call; write_cursor is the
# mechanism. Kept as a named alias so call sites read intentionally.
def advance_cursor(name: str, msg_id: str) -> bool:
    """Advance ``name``'s read cursor to ``msg_id`` (ack). Forward-only.

    Returns True if the cursor moved, False if the ack was a no-op because
    ``msg_id`` is at or before the current cursor in the global log order
    (re-ack / older id). The forward-only guard prevents a rewind that would
    re-surface already-consumed messages: ``scan_unread`` returns ``to==name``
    messages AFTER the cursor, so moving the cursor backward would mark
    consumed mail unread again. A reset is an explicit cursor-file delete, never
    a backward ack.

    If the current cursor's id has rotated out of the retained log (so its
    position is unknowable), the advance is allowed: we cannot prove a rewind,
    and the absent/unresolvable-cursor scan rescans retained segments anyway.
    """
    current = read_cursor(name)
    if current is None or current == msg_id:
        if current == msg_id:
            return False  # already exactly here
        write_cursor(name, msg_id)
        return True

    # Compare positions in the global log order. Unique ids -> first match.
    ids = [m.id for m in iter_messages()]
    try:
        cur_pos = ids.index(current)
    except ValueError:
        # Current cursor rotated out / unresolvable: allow the advance.
        write_cursor(name, msg_id)
        return True
    try:
        new_pos = ids.index(msg_id)
    except ValueError:
        # Target not in retained log; caller (cmd_bus_ack) validates existence
        # first, so this is an unexpected race. Be conservative: do not rewind.
        return False
    if new_pos <= cur_pos:
        return False  # at or before the cursor -> no rewind
    write_cursor(name, msg_id)
    return True


def scan_unread(
    name: str,
    *,
    warn: bool = True,
    exclude_from: Optional[set[str]] = None,
    aliases: Iterable[str] = (),
) -> list[Envelope]:
    """Return messages addressed to ``name`` after its cursor, oldest -> newest.

    ``aliases`` are this consumer's retired addresses. They widen which messages
    count as mine (any ``to`` in ``{name, *aliases}``), and EACH address is
    bounded by its OWN cursor. That per-address watermark is what makes a rename
    safe, because the cursor filename IS the address: collapsing the set onto one
    watermark would either strand mail sent to the never-consumed new address
    (it sits before the old address's cursor) or replay mail already consumed
    under the old one. Neither is acceptable, and no single position avoids both.

    Resolving this on the read side rather than by migrating the cursor file
    keeps read-only surfaces (``whoami``, ``notify-self``) read-only, and leaves
    no check-then-write for two drains to race.

    Still ONE bus scan regardless of alias count, which the every-turn
    notify-self hook depends on for its budget.

    If the cursor is absent or its message-id is not found in any retained
    segment (rotated out / deleted), all retained messages to ``name`` are
    returned rather than silently skipping unprocessed mail.

    ``exclude_from`` drops any message whose sender matches (by ``from`` name or
    ``from_session``). This is the sender-exclusion for a ``to_kind=project``
    broadcast read - a project member must not drain its own broadcast back
    (cv-d54ddd45). By-name reads pass ``exclude_from=None`` (a direct address is
    never a self-echo). Default ``None`` is byte-for-byte the prior behavior.
    """
    mine = {name, *aliases}
    msgs = list(iter_messages(warn=warn))
    excl = exclude_from or set()

    def _mine(m: Envelope) -> bool:
        if m.to not in mine:
            return False
        if excl and (m.from_ in excl or (m.from_session and m.from_session in excl)):
            return False
        return True

    # An address whose cursor is unset, or whose cursor id rotated out of the
    # retained log, counts as never-consumed: everything retained for it is
    # returned rather than silently skipped.
    retained = {m.id for m in msgs}
    cursors = {n: read_cursor(n) for n in mine}
    passed = {n: (c is None or c not in retained) for n, c in cursors.items()}
    # A list per id, not one owner: a drain advances every alias to the SAME id,
    # so the shared case is the common one, and keeping a single owner would
    # leave the other address never-passed and strand everything sent to it.
    ends_at: dict[str, list[str]] = {}
    for n, c in cursors.items():
        if c in retained:
            ends_at.setdefault(c, []).append(n)

    out: list[Envelope] = []
    for m in msgs:
        if _mine(m) and passed[m.to]:  # _mine first: it guards the passed lookup
            out.append(m)
        for owner in ends_at.get(m.id, ()):
            passed[owner] = True
    return out
