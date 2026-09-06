"""In-package OS notification dispatch (US2 internalization of notify.sh).

The ``fno notify`` verb formerly sourced ``scripts/lib/notify.sh`` and called
its ``notify`` bash function, so the verb 127-failed on a bare ``pip install
fno`` where ``scripts/`` is absent. This module reimplements the same dispatch
in-package so the verb runs from the installed wheel with no repo-root
dependency.

Behavior parity with the former bash on the success path (the byte-for-byte
in-clone invariant): macOS uses ``osascript``; Linux uses ``notify-send``; the
underlying tool's own failure is swallowed (the bash used ``|| true``). The one
intentional divergence is the no-tool path: the bash silently returned 0 when
neither tool was present, but AC2-FR requires a loud, non-zero, one-line
degrade (never a silent no-op). ``scripts/lib/notify.sh`` is kept on disk for
in-clone bash sourcers; only the Python verb is re-pointed here.

x-5f06 adds the second leg: every notice is appended to the project journal as
an ``operator_notice`` event, which a status sink with
``events = ["operator_notice"]`` carries off the host to a remote operator.
The return contract widens accordingly: 0 when at least one channel accepted
the message (a local tool dispatched, or the event landed and an enabled sink
routes it); non-zero only when no channel exists at all. The AC2-FR shape is
unchanged - the code stays an honest channel count, in both directions.
"""
from __future__ import annotations

import logging
import os
import platform
import shutil
import subprocess

_log = logging.getLogger(__name__)

_NO_TOOL_MSG = (
    "fno inbox notify: no OS notification tool available (need osascript on "
    "macOS or notify-send on Linux); notification not sent."
)


def _suppressed() -> bool:
    """True when this process must not reach the operator's screen.

    ``fno.hermetic.neutralise`` stamps ``FNO_TEST_HERMETIC=1`` on every child it
    builds, so one check covers the pytest, shell and cargo trees. ``daemon.rs``
    already gates its own ``fno inbox notify`` spawn off in tests; this is the
    Python half of the same rule.

    Deliberately checked at the DISPATCH and not at the top of
    :func:`send_notification`: the tool-availability contract (AC2-FR, a loud
    non-zero when neither notifier exists) is what its caller branches on, and
    short-circuiting the whole function would answer "delivered" on a host that
    has no notifier at all. Suppressing only the subprocess keeps the returned
    code honest about the CHANNEL while the toast never fires.
    """
    return os.environ.get("FNO_TEST_HERMETIC") == "1"


def _dispatch_local(title: str, message: str) -> bool:
    """Fire the OS toast. True when a local tool was available to attempt it.

    The tool's own failure is swallowed (the former bash ``|| true``): a
    notification is never load-bearing. Under ``FNO_TEST_HERMETIC`` the
    subprocess is skipped but a present tool still counts as a channel, so a
    caller that branches on the code takes the same path a real run would.
    """
    if platform.system() == "Darwin":
        # osascript ships with macOS; best-effort like the former bash helper.
        # `osascript -e` compiles its argument as AppleScript, so a `"` or `\` in
        # title/message could terminate the string literal and inject script.
        # Escape backslash first, then double-quote (gemini PR #515 security).
        # Always pass a timeout so a hung osascript can't wedge the caller, and
        # swallow any subprocess failure (missing tool, timeout) - the former
        # bash used `|| true`, and a notification must never be load-bearing.
        esc_message = message.replace("\\", "\\\\").replace('"', '\\"')
        esc_title = title.replace("\\", "\\\\").replace('"', '\\"')
        if _suppressed():
            return True
        try:
            subprocess.run(
                [
                    "osascript",
                    "-e",
                    f'display notification "{esc_message}" with title "{esc_title}"',
                ],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=5,
            )
        except Exception:
            pass
        return True

    if shutil.which("notify-send"):
        # notify-send takes argv directly (no shell/AppleScript injection vector),
        # but still bound it with a timeout and swallow failures for parity.
        if _suppressed():
            return True
        try:
            # argv-fence: exempt (a notification body, not a spawn seed: a
            # leading-dash body drops one toast, it never idles a worker).
            subprocess.run(
                ["notify-send", title, message],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=5,
            )
        except Exception:
            pass
        return True

    return False


def _sink_routes(event_type: str) -> bool:
    """True when at least one enabled status sink routes ``event_type``."""
    try:
        from fno.config import load_settings

        return any(
            sink.enabled and event_type in sink.events
            for sink in load_settings().status_sinks
        )
    except Exception as exc:  # noqa: BLE001 - a config read must not break the toast
        _log.warning("notify: sink check failed: %s", exc)
        return False


def _emit_operator_notice(title: str, message: str, pointer: str) -> bool:
    """Append the ``operator_notice`` journal row (the sink lane's input).

    Returns True when the row landed. Best-effort beside the local toast - a
    journal failure must never turn a delivered toast into an error - but the
    count in :func:`send_notification` reads it, so a host with no local tool
    still answers honestly when this is the only channel.
    """
    if _suppressed():
        return False
    try:
        from fno.events import append_event, operator_notice

        append_event(
            operator_notice(title=title, body=message, pointer=pointer, source="python")
        )
        return True
    except Exception as exc:  # noqa: BLE001 - same best-effort as the toast
        _log.warning("notify: operator_notice append failed: %s", exc)
        return False


def send_notification(title: str, message: str, pointer: str = "") -> tuple[int, str]:
    """Dispatch an operator notice: local toast, journal row for the sink lane.

    Returns ``(exit_code, error_message)``: ``(0, "")`` when at least one
    channel accepted the message - a local tool dispatched, or the journal row
    landed and an enabled status sink routes ``operator_notice`` - and
    ``(1, msg)`` when no channel exists, so the caller can degrade loudly
    instead of no-opping silently (AC2-FR). ``pointer`` names the verb that
    shows the durable state (e.g. ``fno inbox outstanding``); a notice is a
    pointer to the queue, never a copy of it.
    """
    title = title or "target"
    message = message or "Complete"

    local = _dispatch_local(title, message)
    row = _emit_operator_notice(title, message, pointer)
    if local:
        return 0, ""
    # No local tool: the sink lane is the only possible channel, so the only
    # case where the config read buys an answer is this one. A host with a
    # working notifier never pays a settings load here.
    if row and _sink_routes("operator_notice"):
        return 0, ""
    return 1, _NO_TOOL_MSG
