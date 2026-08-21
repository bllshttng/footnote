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

Three sidecar states, and only one of them ever expires:

- **absent** - no clock was written for this flag, so it is not a busy-mode
  hold at all. It is a policy stamped by ``fno agents register
  --delivery-policy bus-only``, which has no clock by construction. Never
  lapses. See :func:`lapsed` for why lifting it instead was tried and reversed.
- **``until: null``** - the same permanent policy, recorded explicitly so the
  DND column can say "held" rather than leaving the operator to guess.
- **``until: <iso8601>``** - a timed busy-mode hold. Lapses at that instant,
  and only this state lapses.
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
    ``AttributeError`` from the resolver escape into ``fno agents mail notify-self``,
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
    """Record a policy that never expires (the hand-stamped ``bus-only``).

    Not needed for enforcement - an absent clock already never lapses. This is
    for the RENDER: it lets the DND column distinguish a deliberate permanent
    policy from a row nobody has looked at, on rows written from here on.
    """
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
    caller is ``fno agents mail notify-self``, which fires on every
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


def lapsed(handle) -> bool:
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
    ``fno agents mail notify-self`` tidies the stale flag when it gets there. So a
    lost clock costs a stall bounded by the operator's next prompt, never a
    lost message. Auto-expire stays a safety property by having two carriers
    that do not depend on this file surviving: the detached release timer, and
    that turn-boundary tidy.

    Pure read. It never mutates the registry, so it cannot deadlock a caller
    that already holds the registry lock and cannot raise into the gate.
    """
    hold = read_any(handle)
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
    # Report what the WRITE did, not that the attempt was made. `set_policy`
    # returns False for a registry it could not read or a row it never found,
    # and returning True over that is an instrument reporting success on its
    # own no-op path. The caller learns nothing, and the flag is still set.
    return set_policy(handle, None)


def candidate_keys(target) -> tuple:
    """Every key a clock for ``target`` could sit under, from any address form.

    The ONE resolution rule every READER shares. ``target`` is a registry row
    or any token that addresses one: a name, a short id, a full session id, or
    the canonical handle.

    This exists because three readers each keyed differently and each looked
    correct on claude, where the name, the short id and the canonical handle
    tend to coincide. On codex they do not, so the DND column read a spawn
    label, the bounce read whatever the sender typed, and the gate read three
    forms no writer uses. Same defect, three places, one fix.
    """
    if hasattr(target, "harness_session_id") or hasattr(target, "name"):
        return addresses(target)
    token = str(target)
    entry = resolve_entry(token)
    if entry is None:
        return (token,)
    keys = addresses(entry)
    return keys if token in keys else (token,) + keys


def read_any(target) -> Optional[Hold]:
    """The clock for ``target`` under whichever of its keys carries one."""
    for key in candidate_keys(target):
        clock = read(key)
        if clock is not None:
            return clock
    return None


def dnd_label(handle) -> Optional[str]:
    """What the DND column shows for a row whose flag IS ``bus-only``.

    Defined in terms of :func:`lapsed`, so the column and the delivery gate can
    never disagree about whether mail is moving. That matters more than the
    string: a column that says one thing while the gate does another is worse
    than no column, and this node exists because state was modelled and never
    shown.

    ``None`` when mail flows despite the flag, which is the lapsed-timed-hold
    case and the one state where the flag is stale. ``"held"`` when the hold
    has no end to show. A duration whenever there is one, because a hold with
    no visible end is what the operator asked to avoid.
    """
    if lapsed(handle):
        return None
    return remaining_label(handle) or "held"


def remaining_label(handle) -> Optional[str]:
    """The compact remaining-time string alone, or None when there is no clock.

    Callers rendering the operator-facing column want :func:`dnd_label`, which
    answers the question the column asks. This one answers only "how long is
    left", and returns None for a row that is held with no end recorded.
    """
    hold = read_any(handle)
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


def bounce_reason(recipient) -> Optional[str]:
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


def addresses(entry) -> tuple:
    """Every token a hold clock for ``entry`` could be filed under.

    ONE matching rule, shared by the resolver, the policy write, and the
    delivery gate's expiry check. The canonical handle is in here because every
    WRITER keys by it: ``fno agents mail hold`` arms at ``canonical_handle(session_id)``
    and the release and the turn-boundary tidy read the same key. For a claude
    row that also happens to be ``short_id``, which is how the omission stayed
    invisible. A codex ``short_id`` is a daemon worker key and its
    ``harness_session_id`` is the full id, so neither is the first-eight the
    clock sits under, and a gate reading only those three looked for a codex
    hold in a place nothing ever writes.
    """
    from fno.harness_identity import canonical_handle

    session_id = getattr(entry, "harness_session_id", None)
    tokens = [
        getattr(entry, "name", None),
        getattr(entry, "short_id", None),
        session_id,
    ]
    if session_id:
        try:
            tokens.append(canonical_handle(session_id))
        except Exception:  # noqa: BLE001 - a malformed id contributes no address
            pass
    ordered: list = []
    for token in tokens:
        if token and token not in ordered:
            ordered.append(token)
    return tuple(ordered)


def resolve_entry(handle: str):
    """The registry row ``handle`` addresses, freshly loaded, or None.

    Load it AFTER the policy is cleared, never before. ``_deliver_live`` and
    every lane under it consult ``_delivery_policy_refusal``, and that gate
    reads ``delivery_policy`` straight off the object it is handed. A row
    captured while the flag was still set carries a stale ``bus-only`` and the
    release is refused by the very gate that held the mail.
    """
    from fno.agents.registry import load_registry

    try:
        entries = load_registry()
    except Exception:  # noqa: BLE001 - a registry hiccup is a missed lane, not a crash
        return None
    for entry in entries:
        if handle in addresses(entry):
            return entry
    return None


def set_policy(handle: str, policy: Optional[str]) -> bool:
    """Stamp ``delivery_policy`` on the registry row addressed by ``handle``.

    Returns whether the DESIRED STATE now holds, not whether a row was edited.
    So a handle with no registry row returns True: no row carries a policy, so
    the caller's "clear it" is already satisfied and there is nothing to strand.
    False means only that the registry could not be read or written, which is
    the one case where a flag may still be set behind the caller's back.

    The distinction is load-bearing for :func:`release`, which drops the clock
    on True. Collapsing "no such row" and "registry unreadable" into one False
    kept the clock forever for every handle that has no row, which is every
    sandbox and every handle whose session already exited.

    Separate from ``register_existing_session`` because that verb resolves the
    AMBIENT session, and the release path runs in a detached timer process with
    no ambient identity of its own.
    """
    from fno.agents.registry import update_registry

    def _updater(entries):
        for entry in entries:
            if handle in addresses(entry):
                entry.delivery_policy = policy
                break
        return entries

    try:
        update_registry(_updater)
    except Exception:  # noqa: BLE001 - a registry hiccup never breaks the caller
        return False
    return True


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
        f"[fno agents mail] busy mode ended after ~{minutes}m. "
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
    lines.append('\n[fno agents mail] to answer one: fno agents mail reply --to <id> --body "..."')
    return "\n".join(lines)


def release(handle: str, *, held_for_s: int = 0) -> dict:
    """End the hold and deliver what it held, with no operator input.

    THE DRAIN TRIGGER THAT IS NOT THE OPERATOR. Mail otherwise drains at
    exactly two moments, ``inject-mail-drain-session-start.sh`` (SessionStart)
    and ``inject-mail-notify.sh`` (UserPromptSubmit), and both need the
    operator to type. A hold with only those two triggers converts an
    interruption into a stall, which is worse than the interruption.

    Order is load-bearing. The flag is cleared FIRST, and the registry row is
    re-read only after that, because every lane consults
    ``_delivery_policy_refusal`` on the object it is handed. Release with the
    flag still set, or with a row captured before it was cleared, and the
    delivery is refused by the very gate that held the mail.

    Delivery goes through ``_deliver_live``, the lane DISPATCHER, so a codex,
    gemini or mux-hosted operator gets the same drain trigger a claude one
    does. Wiring it to the claude injector alone made this a producer on one of
    N lanes: the hold lifted on time and delivered nothing, which is the stall
    this feature exists to prevent wearing the costume of a working one.

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

    # Clear the FLAG first, and drop the clock only once that write succeeded.
    # The old order discarded set_policy's result and cleared the clock either
    # way, which turned a partial failure into a worse state than the failure:
    # a bus-only row with no clock never lapses, so a hold that failed to lift
    # became PERMANENT, and no automatic path clears it - `tidy_lapsed` needs a
    # clock it no longer has.
    #
    # Keeping the clock on a failed write also lets this delivery through. The
    # gate reads a bus-only row with a LAPSED clock as not-holding, so the
    # digest still lands; the same row with no clock is refused outright.
    policy_cleared = set_policy(handle, None)
    if policy_cleared:
        clear(handle)

    try:
        messages = scan_unread(handle, warn=False)
    except Exception:  # noqa: BLE001 - a bus read failure is not a delivery failure
        messages = []
    survivors = dedupe(messages)
    held_count = len(messages)
    deduped_count = held_count - len(survivors)

    outcome = "empty"
    miss_reason: list = []
    if survivors:
        from fno.agents.dispatch import _deliver_live

        digest = render_digest(handle, survivors, held_for_s)
        # Route through the LANE DISPATCHER, not the claude injector. Delivering
        # via `_mail_inject_claude` alone made the release a producer on one of
        # N lanes: a codex or gemini operator, or a mux-hosted pane, armed a
        # hold that lifted on time and then delivered nothing, so their mail
        # waited for them to type. That is the stall busy mode exists to
        # prevent, wearing the costume of a working feature.
        #
        # The entry is resolved HERE, after the flag was cleared above, because
        # every lane under this call re-checks the delivery policy on the object
        # it is handed.
        entry = resolve_entry(handle)
        delivered = False
        if entry is None:
            miss_reason.append("no-registry-row")
        else:
            try:
                delivered = _deliver_live(
                    entry, digest, "fno-mail-hold", reason_out=miss_reason
                )
            except Exception:  # noqa: BLE001 - report the miss, never crash the timer
                delivered = False
                miss_reason.append("deliver-raised")
        if delivered:
            outcome = "delivered"
            for message in messages:
                advance_cursor(handle, getattr(message, "id", ""))
        else:
            outcome = "inject-missed"

    # Carry the lane's OWN reason for a miss. "inject-missed" alone cannot
    # separate a dead daemon from a recipient with no row from a lane that
    # never ran, and those need different repairs.
    events.emit(
        "mail_hold_released",
        handle=handle,
        held_count=held_count,
        deduped_count=deduped_count,
        held_for_s=held_for_s,
        outcome=outcome,
        miss_reason=(miss_reason[0] if miss_reason else None),
        policy_cleared=policy_cleared,
    )
    return {
        "handle": handle,
        "held_count": held_count,
        "deduped_count": deduped_count,
        "held_for_s": held_for_s,
        "outcome": outcome,
        "miss_reason": (miss_reason[0] if miss_reason else None),
        # Whether the FLAG actually came off. A caller that asked for the hold
        # to stop needs to know whether it stopped; reporting delivery while
        # the row still refuses is success on a no-op path.
        "policy_cleared": policy_cleared,
    }
