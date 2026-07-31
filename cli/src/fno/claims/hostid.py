"""Stable machine identity for claim liveness.

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
macOS, ``/etc/machine-id`` plus the pid-namespace inode on Linux) and falls
back to ``gethostname()`` where neither is readable, which is no worse than
the previous behavior.

It travels in its OWN claim field rather than replacing ``host``. Overwriting
``host`` would make a pre-change reader - a still-running old binary during a
rolling upgrade - compare a machine id against its ``gethostname()``, miss, and
classify a LIVE claim as stale, which is stealable: exactly the bug this fixes,
reintroduced from the other side. An additive field reads as absent on old
readers, so they behave precisely as they do today. This mirrors how ``harness``
was added.

Cross-host opacity is preserved deliberately: a claim filed on a genuinely
different machine carries that machine's id, matches neither arm of
``is_same_machine``, and stays opaque exactly as the design doc specifies.

Mirrored by ``machine_id`` / ``is_same_machine`` in ``crates/fno-agents/src/claims.rs``.
"""
from __future__ import annotations

import os
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
    base = ""
    for path in _LINUX_MACHINE_ID_FILES:
        try:
            value = Path(path).read_text(encoding="utf-8").strip()
        except OSError:
            continue
        if value:
            base = value
            break
    if not base:
        return ""
    # Containers built from one image share /etc/machine-id while holding
    # INDEPENDENT pid namespaces. Identity scopes PID-reuse detection, so
    # without the namespace two such containers sharing a claims root would
    # read each other's pids as local: a dead foreign claim would classify
    # LIVE forever (a wedge) instead of staying opaque, which is the
    # cross-machine guarantee coordination.md states.
    try:
        namespace = os.stat("/proc/self/ns/pid").st_ino
    except OSError:
        return base
    return f"{base}:{namespace}"


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


def is_same_machine(host: Optional[str], machine: Optional[str]) -> bool:
    """Was this claim written on THIS machine?

    ``machine`` is the claim's ``machine_id`` and is authoritative whenever it
    is present. It is absent only on a claim written before that field existed,
    and those fall back to the old hostname compare - which is the buggy
    comparison this module exists to replace, but reproducing it exactly is the
    point: a pre-change claim must classify exactly as it does today, no worse.

    Dispatching on field presence rather than OR-ing the two candidates keeps a
    hostname that happens to equal some other machine's id from matching.

    ``machine`` is REQUIRED and deliberately has no default. A default let two
    callers keep compiling while silently taking the pre-change fallback for
    every claim, which is the fix quietly not applying on those paths.
    """
    if machine:
        return machine == machine_id()
    if not host:
        return False
    return host == hostname()
