"""Stable machine identity for the claim ``host`` field.

``socket.gethostname()`` is not a stable machine identity. On macOS with
``scutil --get HostName`` unset, it is derived from whatever DHCP/DNS most
recently supplied and falls back to ``<LocalHostName>.local``; it flips on
network join, VPN, and sleep/wake. A roaming laptop therefore changes the
answer mid-session.

The claim ``host`` field is doing PID-reuse SCOPING - "is this pid namespace
mine?" - not display. Keying that on a moving string made a live holder read
as cross-host, which short-circuits ``staleness.is_live`` before the pid check
and drops the claim to STALE. STALE is recoverable, so the claim became
stealable out from under a working session: duplicate work, duplicate PR.

``machine_id()`` prefers the OS's own stable identifier (IOPlatformUUID on
macOS, ``/etc/machine-id`` on Linux) and falls back to ``gethostname()`` where
neither is readable, which is no worse than the previous behavior.

Cross-host opacity is preserved deliberately: a claim filed on a genuinely
different machine carries that machine's id, matches neither arm of
``is_same_machine``, and stays opaque exactly as the design doc specifies.

Mirrored by ``machine_id`` / ``is_same_machine`` in ``crates/fno-agents/src/claims.rs``.
"""
from __future__ import annotations

import platform
import re
import socket
import subprocess
from functools import lru_cache
from pathlib import Path
from typing import Optional

# systemd writes the first; dbus the second on systems without systemd.
_LINUX_MACHINE_ID_FILES = ("/etc/machine-id", "/var/lib/dbus/machine-id")

_IOREG_UUID_RE = re.compile(r'"IOPlatformUUID"\s*=\s*"([^"]+)"')


def hostname() -> str:
    """``socket.gethostname()``, or "" when it cannot be read.

    Empty never equals a recorded non-empty host, so an unreadable hostname
    fails toward "not this machine" - the recoverable direction, matching the
    posture the rest of the claims code takes on unverifiable state.
    """
    try:
        return socket.gethostname()
    except OSError:
        return ""


def _macos_platform_uuid() -> str:
    """IOPlatformUUID: per-machine, survives renames, reinstalls, and roaming."""
    try:
        proc = subprocess.run(
            ["ioreg", "-rd1", "-c", "IOPlatformExpertDevice"],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    match = _IOREG_UUID_RE.search(proc.stdout or "")
    return match.group(1) if match else ""


def _linux_machine_id() -> str:
    for path in _LINUX_MACHINE_ID_FILES:
        try:
            value = Path(path).read_text(encoding="utf-8").strip()
        except OSError:
            continue
        if value:
            return value
    return ""


@lru_cache(maxsize=1)
def machine_id() -> str:
    """A stable identifier for this machine; falls back to ``hostname()``.

    Cached for the process lifetime: the macOS arm shells out to ``ioreg``, and
    a claims sweep reads many lockfiles against one machine identity. Tests
    that simulate a different machine call ``machine_id.cache_clear()``.
    """
    system = platform.system()
    if system == "Darwin":
        resolved = _macos_platform_uuid()
    elif system == "Linux":
        resolved = _linux_machine_id()
    else:
        resolved = ""
    return resolved or hostname()


def is_same_machine(recorded: Optional[str]) -> bool:
    """Was ``recorded`` (a claim's ``host``) written on THIS machine?

    Two arms. The first is the real one: the stable machine id. The second
    accepts a bare hostname for claims written before this field carried a
    machine id - additive, so a legacy claim is never worse off than it was,
    and a genuinely remote host matches neither.
    """
    if not recorded:
        return False
    if recorded == machine_id():
        return True
    return recorded == hostname()
