"""Busy mode: the CLOCK behind a ``delivery_policy = "bus-only"`` hold.

The HOLD itself is not here. It is the registry flag (schema v14) that
:func:`fno.agents.dispatch._delivery_policy_refusal` already enforces before
any transport call on every injector lane. This module owns only the two
things that flag cannot express on its own: WHEN the hold ends, and whether
it ends at all.

That split is deliberate. Putting an expiry on the registry row would mean a
schema bump and a new field threaded through the Rust ``RegistryEntry`` at
nine construction sites, for one timestamp. So the clock is a sidecar and the
flag stays the sole enforcement authority: a reader asking "is mail held"
reads the flag, a reader asking "until when" reads the sidecar.

Three sidecar states, and the third is the one that matters:

- **absent** - no clock was ever written for a set flag. The self-heal LIFTS
  the hold. See :func:`lapsed` for why the fail-open direction is the safe one.
- **``until: null``** - a deliberate permanent policy, stamped by
  ``fno agents register --delivery-policy bus-only``. Never lapses. This state
  exists so the fail-open above cannot destroy a manual stamp that has no
  clock by design.
- **``until: <iso8601>``** - a timed busy-mode hold. Lapses at that instant.
"""
from __future__ import annotations

import json
import math
import os
import tempfile
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Optional

from fno import paths

#: Sidecar written for a policy that is deliberately permanent, so an absent
#: sidecar can safely mean "no clock, lift it" without destroying a hand stamp.
NO_EXPIRY = {"until": None, "window_s": None}

DEFAULT_MINUTES = 5


@dataclass(frozen=True)
class Hold:
    """One session's hold clock. ``until`` is None for a permanent policy."""

    handle: str
    until: Optional[datetime]
    window_s: Optional[int]


def hold_dir() -> Path:
    """``~/.fno/mail-hold/`` - one file per handle.

    A subfolder rather than a top-level state-root file: the contents are
    session-keyed and short-lived, which is exactly the split
    ``docs/state-root-inventory.md`` draws.
    """
    return paths.state_dir() / "mail-hold"


def hold_path(handle: str) -> Path:
    return hold_dir() / f"{handle}.json"


def _now() -> datetime:
    return datetime.now(timezone.utc)


def _parse(stamp: str) -> Optional[datetime]:
    try:
        parsed = datetime.fromisoformat(stamp.replace("Z", "+00:00"))
    except (AttributeError, TypeError, ValueError):
        return None
    return parsed if parsed.tzinfo else parsed.replace(tzinfo=timezone.utc)


def read(handle: str) -> Optional[Hold]:
    """This handle's clock, or None when there is no readable one.

    A corrupt or unparseable file reads as None, the same as an absent one.
    Both mean "no clock", and neither is evidence that a hold is running.

    Never raises. The catch is deliberately broad because resolving the
    directory runs the whole path resolver, which loads settings and can fail
    in ways a file read cannot - a narrow ``(OSError, ValueError)`` here let an
    ``AttributeError`` from the resolver escape into ``fno mail notify-self``,
    which runs on every ``UserPromptSubmit``. Busy mode must never be able to
    break the turn-boundary render: an unreadable clock means the mail flows.
    """
    try:
        raw = json.loads(hold_path(handle).read_text(encoding="utf-8"))
    except Exception:  # noqa: BLE001 - see above; a clock read never breaks a caller
        return None
    if not isinstance(raw, dict):
        return None
    until_raw = raw.get("until")
    until = _parse(until_raw) if isinstance(until_raw, str) else None
    if isinstance(until_raw, str) and until is None:
        return None
    window = raw.get("window_s")
    return Hold(
        handle=handle,
        until=until,
        window_s=window if isinstance(window, int) else None,
    )


def _write(hold: Hold) -> Hold:
    """Atomic replace, so a reader never catches a half-written clock."""
    directory = hold_dir()
    directory.mkdir(parents=True, exist_ok=True)
    payload = json.dumps(
        {
            "until": hold.until.strftime("%Y-%m-%dT%H:%M:%SZ") if hold.until else None,
            "window_s": hold.window_s,
        }
    )
    fd, tmp = tempfile.mkstemp(dir=str(directory), suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle_file:
            handle_file.write(payload + "\n")
        os.replace(tmp, hold_path(hold.handle))
    except OSError:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise
    return hold


def arm(handle: str, minutes: int = DEFAULT_MINUTES) -> Hold:
    """Start (or restart) a timed hold of ``minutes`` for ``handle``."""
    window_s = max(1, int(minutes * 60))
    return _write(
        Hold(handle=handle, until=_now() + timedelta(seconds=window_s), window_s=window_s)
    )


def arm_permanent(handle: str) -> Hold:
    """Record a policy that never expires (the hand-stamped ``bus-only``)."""
    return _write(Hold(handle=handle, until=None, window_s=None))


def clear(handle: str) -> None:
    """Remove the clock. Absent is success, not an error."""
    try:
        hold_path(handle).unlink()
    except OSError:
        pass


def extend(handle: str) -> Optional[Hold]:
    """Push a live timed hold out by its own window and return the new clock.

    Returns None when there is no live timed hold to extend (no clock, a
    permanent policy, or one already lapsed). This is the idle re-arm: the
    caller is ``fno mail notify-self``, which fires on every
    ``UserPromptSubmit``, so "the operator typed" restarts the idle window
    without any new hook.
    """
    hold = read(handle)
    if hold is None or hold.until is None or hold.window_s is None:
        return None
    if hold.until <= _now():
        return None
    return _write(
        Hold(
            handle=handle,
            until=_now() + timedelta(seconds=hold.window_s),
            window_s=hold.window_s,
        )
    )


def lapsed(handle: str) -> bool:
    """True when a TIMED hold for ``handle`` has run out. Only ever timed.

    An absent or unreadable clock reads as NOT lapsed, which leaves the flag
    exactly as it behaved before busy mode existed. The alternative was tried
    and is wrong: lifting a flag that has no clock silently revokes the
    delivery policy of every row stamped by ``fno agents register
    --delivery-policy bus-only`` before this file existed, and a row so stamped
    has no clock by construction. That is a shipped no-paste guarantee broken
    on rows nobody touched.

    The fear that argument answers - a hold whose timer died holding mail
    forever - does not describe this design. Held mail is durable on the bus.
    It surfaces at the recipient's next SessionStart or turn boundary, and
    ``fno mail notify-self`` tidies the stale flag when it gets there. So a
    lost clock costs a stall bounded by the operator's next prompt, never a
    lost message. Auto-expire stays a safety property by having two carriers
    that do not depend on this file surviving: the detached release timer, and
    that turn-boundary tidy.

    Pure read. It never mutates the registry, so it cannot deadlock a caller
    that already holds the registry lock and cannot raise into the gate.
    """
    hold = read(handle)
    if hold is None or hold.until is None:
        return False
    return hold.until <= _now()


def tidy_lapsed(handle: str) -> bool:
    """Clear a timed hold that has run out, flag and clock together.

    The delivery gate deliberately stays a pure read, so nothing on the send
    path clears a stale ``bus-only`` flag. This is where it gets cleared: a
    turn boundary, where the registry lock is free and a write is safe.

    Only a TIMED hold is tidied. A clock reading ``until: null`` is a
    deliberate permanent policy, and an absent clock cannot be told apart from
    a row that never had one, so neither is touched here.
    """
    clock = read(handle)
    if clock is None or clock.until is None or clock.until > _now():
        return False
    clear(handle)
    set_policy(handle, None)
    return True


def remaining_label(handle: str) -> Optional[str]:
    """A compact remaining-time string for the DND column, or None.

    None whenever nothing is being held right now, so the column renders the
    same for "no hold" and "a hold that has already lapsed" - both are states
    in which mail flows. A hold with no visible end is the thing the operator
    asked to avoid, so a live hold always renders a duration, never a bare yes.
    """
    hold = read(handle)
    if hold is None:
        return None
    if hold.until is None:
        return "held"
    seconds = (hold.until - _now()).total_seconds()
    if seconds <= 0:
        return None
    if seconds < 60:
        return f"~{int(seconds)}s"
    return f"~{math.ceil(seconds / 60)}m"


def bounce_reason(recipient: str) -> Optional[str]:
    """The busy-mode receipt a sender reads, or None when no hold is running.

    A refusal that says nothing is the defect this replaces. A sender that gets
    silence cannot tell a hold from a dead bus, so it re-sends, which is how one
    held message becomes five. The generic ``bus-only`` receipt is no better
    here: it promises the recipient's next turn boundary, and a busy-mode hold
    exists precisely because that turn boundary is not coming.

    So the reason names the recipient, the state, and when the message actually
    lands. Returns None for a permanent hand-stamped policy, which really does
    surface at a turn boundary and already has an accurate receipt.
    """
    label = remaining_label(recipient)
    if label is None or label == "held":
        return None
    return (
        f"held: {recipient} is in do-not-disturb, lifts in "
        f"{label.lstrip('~')} and delivers itself then"
    )


def set_policy(handle: str, policy: Optional[str]) -> bool:
    """Stamp ``delivery_policy`` on the registry row named ``handle``.

    Returns True when a row was found and written. Separate from
    ``register_existing_session`` because that verb resolves the AMBIENT
    session, and the release path runs in a detached timer process that has no
    ambient identity of its own.
    """
    from fno.agents.registry import update_registry

    found = [False]

    def _updater(entries):
        for entry in entries:
            if handle in (entry.name, entry.short_id, entry.harness_session_id):
                entry.delivery_policy = policy
                found[0] = True
                break
        return entries

    try:
        update_registry(_updater)
    except Exception:  # noqa: BLE001 - a registry hiccup never breaks the caller
        return False
    return found[0]


def dedupe(messages: list) -> list[tuple[object, int, list[str]]]:
    """Collapse identical bodies from one sender into one entry with a count.

    Returns ``(representative, count, every_id)`` per survivor, oldest first.
    Every id travels with its survivor because the cursor must still be
    advanced past a suppressed duplicate - a message that is not rendered is
    consumed, never left behind to resurface on the next drain.

    A worker that gets no answer re-sends, so a ten minute hold turns one
    report into five. The dedupe is needed even without a retry: a
    duplicate-delivery rate of 4.1 percent was measured on this bus on
    2026-08-19, with identical body md5 AND identical message id.
    """
    import hashlib

    order: list[tuple[str, str]] = []
    groups: dict[tuple[str, str], list] = {}
    for message in messages:
        body = getattr(message, "body", "") or ""
        digest = hashlib.md5(body.encode("utf-8", "replace")).hexdigest()
        key = (getattr(message, "from_", "") or "", digest)
        if key not in groups:
            groups[key] = []
            order.append(key)
        groups[key].append(message)
    out = []
    for key in order:
        members = groups[key]
        out.append((members[0], len(members), [getattr(m, "id", "") for m in members]))
    return out


def render_digest(handle: str, survivors: list, held_for_s: int) -> str:
    """The one text the release injects. Mirrors the notify-self render."""
    total = sum(count for _, count, _ in survivors)
    minutes = max(1, math.ceil(held_for_s / 60)) if held_for_s else 0
    lines = [
        f"[fno mail] busy mode ended after ~{minutes}m. "
        f"{total} message(s) held for {handle}:"
    ]
    for message, count, _ids in survivors:
        suffix = f"  (x{count} identical, deduped)" if count > 1 else ""
        lines.append(
            f"\n--- from {getattr(message, 'from_', '?')} "
            f"({getattr(message, 'ts', '?')})  id:{getattr(message, 'id', '?')} "
            f"---{suffix}"
        )
        lines.append((getattr(message, "body", "") or "").rstrip("\n"))
    lines.append('\n[fno mail] to answer one: fno mail reply --to <id> --body "..."')
    return "\n".join(lines)


def release(handle: str, *, held_for_s: int = 0) -> dict:
    """End the hold and deliver what it held, with no operator input.

    THE DRAIN TRIGGER THAT IS NOT THE OPERATOR. Mail otherwise drains at
    exactly two moments, ``inject-mail-drain-session-start.sh`` (SessionStart)
    and ``inject-mail-notify.sh`` (UserPromptSubmit), and both need the
    operator to type. A hold with only those two triggers converts an
    interruption into a stall, which is worse than the interruption.

    Order is load-bearing. The flag is cleared FIRST, because step 3 goes
    through the injector, and the injector refuses a ``bus-only`` recipient
    before any transport call - release with the flag still set and the
    delivery is refused by the very gate that held it.

    A missed inject does NOT advance the cursor. The mail stays on the bus and
    the recipient's next turn boundary surfaces it, so a dead daemon degrades
    to today's behavior rather than to a loss.

    Always emits ``mail_hold_released``, including when nothing was held. A
    release path that fires only on a non-empty digest cannot tell a working
    expiry from a dead timer, which is the same absence-is-not-evidence trap
    this whole feature exists to avoid.
    """
    from fno.agents import events
    from fno.bus.cursor import advance_cursor, scan_unread

    set_policy(handle, None)
    clear(handle)

    try:
        messages = scan_unread(handle, warn=False)
    except Exception:  # noqa: BLE001 - a bus read failure is not a delivery failure
        messages = []
    survivors = dedupe(messages)
    held_count = len(messages)
    deduped_count = held_count - len(survivors)

    outcome = "empty"
    if survivors:
        from fno.agents.dispatch import _mail_inject_claude

        digest = render_digest(handle, survivors, held_for_s)
        delivered = False
        try:
            delivered = _mail_inject_claude(handle, digest, sender="fno-mail-hold")
        except Exception:  # noqa: BLE001 - report the miss, never crash the timer
            delivered = False
        if delivered:
            outcome = "delivered"
            for message in messages:
                advance_cursor(handle, getattr(message, "id", ""))
        else:
            outcome = "inject-missed"

    events.emit(
        "mail_hold_released",
        handle=handle,
        held_count=held_count,
        deduped_count=deduped_count,
        held_for_s=held_for_s,
        outcome=outcome,
    )
    return {
        "handle": handle,
        "held_count": held_count,
        "deduped_count": deduped_count,
        "held_for_s": held_for_s,
        "outcome": outcome,
    }
