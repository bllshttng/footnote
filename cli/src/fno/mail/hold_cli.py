"""The ``fno agents mail hold`` verb family: one session's do-not-disturb.

Extracted from ``fno.mail.cli`` (file-budget: that module is shrink-only);
cli.py registers the two commands explicitly, the way it registers
``notify-self``. Everything here is hold-shaped: the manifest reader, the
held-job-mail scan, the self-handle resolver, and the hold / hold-release
verbs.
"""

from __future__ import annotations

import json
import os
import sys
import time
from datetime import datetime, timezone

import typer



def cmd_hold(
    minutes: int = typer.Option(
        None,
        "--minutes",
        "-m",
        help="Idle minutes before the hold lifts by itself (default 5). The "
        "quiet window restarts every prompt and ends at 2x the requested window.",
    ),
    for_minutes: int = typer.Option(
        None,
        "--for",
        help="Wall-clock minutes before the hold lifts. The deadline never moves.",
    ),
    off: bool = typer.Option(
        False, "--off", help="Lift the hold now and deliver what it held."
    ),
    status: bool = typer.Option(
        False, "--status", help="Report the current hold without changing it."
    ),
) -> None:
    """Busy mode: hold this session's incoming mail, and drain it on a timer.

    While the hold is on, mail addressed to this session never pastes into the
    prompt line. It queues durable and the sender gets a receipt saying so.
    Either clock DELIVERS without a new prompt, so a hold whose only drain
    trigger is the operator cannot stall.
    """
    from fno.mail import hold as hold_mod
    from fno.harness_identity import session_identity_key

    from fno.mail.cli import _self_handle_or_exit

    handle, ident = _self_handle_or_exit()
    # Clock key: the collision-free identity key (first-eight collides in one 65.536s window).
    clock_key = session_identity_key(str(getattr(ident, "session_id", "") or ""))

    if minutes is not None and for_minutes is not None:
        sys.stderr.write("error: --minutes and --for are mutually exclusive\n")
        raise typer.Exit(code=2)

    if status:
        # The record, not the gate: the gate's own-pass never refuses your own hold.
        entry = hold_mod.resolve_entry(handle)
        policy = str(getattr(entry, "delivery_policy", None) or "")
        print(hold_mod._hold_transport(["--status-line", handle, "--policy", policy]))
        return

    if off:
        result = hold_mod.release(clock_key, held_for_s=0)
        # Report the FLAG first: a failed registry write leaves mail held
        # while the receipt below says the hold is off.
        if not result["policy_cleared"]:
            sys.stderr.write(
                f"hold NOT off: the registry write failed, so {handle} still "
                "reads bus-only and mail is still held. Retry, or check "
                "`fno agents list` for the row.\n"
            )
            raise typer.Exit(code=1)
        if result["held_count"]:
            print(
                f"hold off: delivered {result['held_count']} held message(s) "
                f"({result['deduped_count']} deduped) - {result['outcome']}"
            )
        else:
            print("hold off: nothing was held")
        return

    wall_clock = for_minutes is not None
    window = (
        for_minutes
        if wall_clock
        else hold_mod.DEFAULT_MINUTES if minutes is None else minutes
    )
    if window < 1:
        flag = "--for" if wall_clock else "--minutes"
        sys.stderr.write(f"error: {flag} must be at least 1\n")
        raise typer.Exit(code=2)

    from fno.agents.registry import register_existing_session

    register_existing_session(
        provider=str(getattr(ident, "harness", "") or ""),
        session_id=str(getattr(ident, "session_id", "") or ""),
        cwd=os.getcwd(),
        delivery_policy="bus-only",
    )
    clock = hold_mod.arm_wall(clock_key, window) if wall_clock else hold_mod.arm(clock_key, window)

    until = clock.until or datetime.now(timezone.utc)
    clock_text = hold_mod.clock_description(clock)
    print(
        f"busy mode on for {handle}: {clock_text}, holds until "
        f"{until.strftime('%H:%M:%S')} UTC ({window}m), then delivers itself."
    )


def cmd_hold_release(
    handle: str = typer.Option(..., "--handle", help="The held session's handle."),
    poll_s: int = typer.Option(
        15, "--poll-s", hidden=True, help="Seconds between clock re-reads."
    ),
) -> None:
    """Sleep until ``handle``'s hold expires, then release it.

    Re-reads the clock on every wake rather than sleeping once to the original
    deadline, so an idle re-arm (the operator typed again) extends the hold
    instead of being overrun by a timer that already committed to a time.

    Exits quietly when the clock disappears or turns permanent: both mean
    someone else took the hold off, and a second release would be a no-op that
    still emitted a release event.
    """
    from fno.mail import hold as hold_mod

    started = time.monotonic()
    while True:
        clock = hold_mod.read(handle)
        if clock is None or clock.until is None:
            return
        remaining = (clock.until - datetime.now(timezone.utc)).total_seconds()
        if remaining <= 0:
            break
        time.sleep(min(remaining, max(1, poll_s)))

    result = hold_mod.release(handle, held_for_s=int(time.monotonic() - started))
    print(json.dumps(result))
