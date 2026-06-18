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
in-clone bash sourcers (e.g. ``scripts/lib/inbox-check.sh``); only the Python
verb is re-pointed here.
"""
from __future__ import annotations

import platform
import shutil
import subprocess


def send_notification(title: str, message: str) -> tuple[int, str]:
    """Dispatch an OS notification.

    Returns ``(exit_code, error_message)``: ``(0, "")`` when a notification
    tool was available and a dispatch was attempted (the tool's own failure is
    best-effort/swallowed, matching the former bash ``|| true``); ``(1, msg)``
    when neither ``osascript`` (macOS) nor ``notify-send`` (Linux) is available,
    so the caller can degrade loudly instead of no-opping silently (AC2-FR).
    """
    title = title or "target"
    message = message or "Complete"

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
        return 0, ""

    if shutil.which("notify-send"):
        # notify-send takes argv directly (no shell/AppleScript injection vector),
        # but still bound it with a timeout and swallow failures for parity.
        try:
            subprocess.run(
                ["notify-send", title, message],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=5,
            )
        except Exception:
            pass
        return 0, ""

    return 1, (
        "fno notify: no OS notification tool available (need osascript on "
        "macOS or notify-send on Linux); notification not sent."
    )
