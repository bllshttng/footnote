"""Thin sidecar client for fno CLI processes.

The CLI never talks to the per-session ``channel_server`` directly —
that subprocess is CC-owned and its stdio is unreachable from outside.
The CLI talks to the per-user sidecar (:mod:`fno.mcp.sidecar`)
via a Unix-socket protocol, and the sidecar routes pokes to the right
channel server connection.

Operations (single-shot, sync SOCK_STREAM with bounded timeout):

- :func:`ping` — confirm the sidecar is up.
- :func:`status` — return the JSON for ``fno mcp status``.
- :func:`send_to_channel` — deliver one envelope to one session.

Lazy-start behavior: every call optionally lazy-spawns the sidecar if
the socket file is absent or unreachable. ``lazy_start=False`` is
useful for probes that must distinguish "sidecar down" from "sidecar
down and we tried to fix it but failed".

Spec references:

- LD3 (lazy-start / lazy-exit), LD5 (Unix-socket sidecar),
  LD17 (``fno mcp restart`` scope = sidecar only),
- AC3-EDGE (first call lazy-starts), AC3-FR (lazy-start failure
  falls back cleanly), AC3-MIDBIND (stale socket from mid-bind-crash).
"""
from __future__ import annotations

import errno
import json
import logging
import os
import socket
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Dict, Optional

from fno.mcp import sidecar as _sidecar

LOG = logging.getLogger("fno.mcp.client")

# Default per-call timeout (seconds). The sidecar is local; sub-100ms
# responses are typical. 5s is the spec-mandated upper bound for probes.
DEFAULT_TIMEOUT = 5.0

# Lazy-start subprocess bind window: spec says the subprocess must bind
# the socket within 5s of spawn (AC3-EDGE + AC3-FR).
LAZY_START_BIND_TIMEOUT = 5.0

# Polling interval while waiting for the lazy-started sidecar to bind.
LAZY_START_POLL_INTERVAL = 0.05


class MCPSidecarUnreachable(RuntimeError):
    """Raised when the sidecar is not reachable and lazy-start failed.

    Carries a short ``reason`` discriminator for stderr WARN +
    ``mcp_server_unreachable`` event payloads.
    """

    def __init__(self, reason: str) -> None:
        super().__init__(f"MCP sidecar unreachable: {reason}")
        self.reason = reason


class MCPSidecarError(RuntimeError):
    """Raised when the sidecar responded with ``ok: false``."""

    def __init__(self, reason: str) -> None:
        super().__init__(f"MCP sidecar error: {reason}")
        self.reason = reason


def socket_path() -> Path:
    """Resolved sidecar socket path (re-exports
    :func:`fno.mcp.sidecar._socket_path` so external callers do
    not need to reach into a private name)."""
    return _sidecar._socket_path()


def state_path() -> Path:
    """Resolved sidecar state-flush path."""
    return _sidecar._state_path()


# ---------------------------------------------------------------------
# Lazy-start
# ---------------------------------------------------------------------


def _wait_for_sidecar(sock_path: Path, *, deadline: float) -> bool:
    """Poll for the sidecar to come up before ``deadline`` (epoch).

    Returns ``True`` if a ``ping`` succeeded before the deadline,
    ``False`` otherwise. Bounded by polling interval; never busy-loops.
    """
    while time.time() < deadline:
        if _sidecar._is_sidecar_running(sock_path, timeout=0.2):
            return True
        time.sleep(LAZY_START_POLL_INTERVAL)
    return False


def _spawn_detached_sidecar(sock_path: Path) -> Optional[subprocess.Popen[Any]]:
    """Spawn the sidecar in a new session (detached from this process).

    Mirror of ``abi-supervisor`` lazy-start. Returns the Popen handle on
    success (caller does NOT wait on it). Returns ``None`` if Python
    can't find itself (extremely unusual; tested defensively).
    """
    python = sys.executable
    if not python:
        return None
    LOG.info("lazy-starting sidecar at %s", sock_path)
    log_path = (
        Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local" / "state"))
        / "fno"
        / "sidecar.log"
    )
    log_path.parent.mkdir(parents=True, exist_ok=True)
    # Open the log file, hand it to Popen as stdout/stderr (Popen dups
    # into the child), then close the parent's fd. Without the close,
    # every lazy-start would leak one fd in the parent process - long
    # running orchestrators that re-trigger lazy-start would eventually
    # exhaust the per-process fd ceiling.
    log_handle = open(log_path, "a", encoding="utf-8")
    # Forward the requested sock_path to the lazy-started sidecar via
    # the FNO_SIDECAR_SOCKET env var. Without this, callers that
    # passed a non-default sock_path would lazy-start a sidecar at the
    # DEFAULT path and then deterministically time out polling the
    # non-default path (codex P2 review on PR #323).
    env = dict(os.environ)
    env["FNO_SIDECAR_SOCKET"] = str(sock_path)
    try:
        proc = subprocess.Popen(
            [python, "-m", "fno.mcp.sidecar"],
            stdin=subprocess.DEVNULL,
            stdout=log_handle,
            stderr=log_handle,
            env=env,
            start_new_session=True,
            close_fds=True,
        )
    except OSError as exc:  # pragma: no cover - defensive
        LOG.warning("failed to lazy-start sidecar: %s", exc)
        log_handle.close()
        return None
    log_handle.close()  # Popen dup'd the fd into the child; parent's copy is now leak-free.
    return proc


def ensure_sidecar(
    *,
    sock_path: Optional[Path] = None,
    timeout: float = LAZY_START_BIND_TIMEOUT,
) -> Path:
    """Return the live sidecar socket path, lazy-starting if needed.

    Raises :class:`MCPSidecarUnreachable` if lazy-start fails to bind
    within ``timeout`` seconds.
    """
    sock_path = sock_path or socket_path()
    if _sidecar._is_sidecar_running(sock_path, timeout=0.3):
        return sock_path
    # The socket file may exist as a stale-from-mid-bind-crash artifact
    # (AC3-MIDBIND). _spawn_detached_sidecar will not delete it; the
    # sidecar's own bootstrap (_bind_socket) handles the unlink+rebind
    # so we don't double-handle.
    proc = _spawn_detached_sidecar(sock_path)
    if proc is None:
        raise MCPSidecarUnreachable("lazy_start_spawn_failed")
    deadline = time.time() + timeout
    if not _wait_for_sidecar(sock_path, deadline=deadline):
        raise MCPSidecarUnreachable("lazy_start_timeout")
    return sock_path


# ---------------------------------------------------------------------
# RPC primitives
# ---------------------------------------------------------------------


def _rpc_once(
    sock_path: Path,
    request: Dict[str, Any],
    *,
    timeout: float = DEFAULT_TIMEOUT,
) -> Dict[str, Any]:
    """One request/response over a fresh socket connection.

    Connection is closed after the response is read. Raises
    :class:`MCPSidecarUnreachable` on connect/timeout failure and
    returns the parsed response dict on success (even if it carries
    ``ok: false`` — the caller decides whether to raise
    :class:`MCPSidecarError`).
    """
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    try:
        try:
            s.connect(str(sock_path))
        except ConnectionRefusedError:
            raise MCPSidecarUnreachable("server_not_listening")
        except FileNotFoundError:
            raise MCPSidecarUnreachable("socket_missing")
        except OSError as exc:
            if exc.errno in (errno.ENOENT, errno.ECONNREFUSED):
                raise MCPSidecarUnreachable("server_not_listening")
            raise MCPSidecarUnreachable(f"connect_error:{exc.errno}")
        try:
            s.sendall((json.dumps(request) + "\n").encode("utf-8"))
        except (BrokenPipeError, ConnectionResetError, OSError):
            raise MCPSidecarUnreachable("write_error")

        # Read one line of response. Sidecar may stream additional
        # frames on a registered-channel connection, but single-shot
        # ops always close after one response.
        buf = bytearray()
        try:
            while True:
                chunk = s.recv(4096)
                if not chunk:
                    break
                buf.extend(chunk)
                if b"\n" in buf:
                    break
        except socket.timeout:
            raise MCPSidecarUnreachable("read_timeout")
        except (ConnectionResetError, OSError):
            raise MCPSidecarUnreachable("read_error")
        if not buf:
            raise MCPSidecarUnreachable("empty_response")
        line, _, _ = buf.partition(b"\n")
        try:
            parsed: Dict[str, Any] = json.loads(line.decode("utf-8"))
            return parsed
        except (json.JSONDecodeError, UnicodeDecodeError) as exc:
            raise MCPSidecarUnreachable(f"parse_error:{exc.__class__.__name__}")
    finally:
        try:
            s.close()
        except OSError:  # pragma: no cover
            pass


# ---------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------


def ping(
    *,
    sock_path: Optional[Path] = None,
    timeout: float = DEFAULT_TIMEOUT,
    lazy_start: bool = False,
) -> Dict[str, Any]:
    """Probe the sidecar with a ``ping``.

    Returns the JSON response (``{"ok": True, "pid": ..., "uptime_seconds": ...}``).
    Raises :class:`MCPSidecarUnreachable` if the probe fails AND
    lazy-start either was not requested or failed.
    """
    sock_path = ensure_sidecar(sock_path=sock_path) if lazy_start else (sock_path or socket_path())
    return _rpc_once(sock_path, {"op": "ping"}, timeout=timeout)


def status(
    *,
    sock_path: Optional[Path] = None,
    timeout: float = DEFAULT_TIMEOUT,
) -> Dict[str, Any]:
    """Return the full sidecar status (``fno mcp status`` JSON shape)."""
    sock_path = sock_path or socket_path()
    return _rpc_once(sock_path, {"op": "status"}, timeout=timeout)


def send_to_channel(
    session_id: str,
    envelope: Dict[str, Any],
    *,
    sock_path: Optional[Path] = None,
    timeout: float = DEFAULT_TIMEOUT,
    lazy_start: bool = True,
) -> None:
    """Route ``envelope`` to the channel server registered under
    ``session_id``.

    Raises :class:`MCPSidecarError` (``reason="channel_not_registered"``
    or ``"channel_write_failed"``) when the sidecar reports a logical
    failure, or :class:`MCPSidecarUnreachable` when the sidecar itself
    is unreachable.
    """
    sock_path = ensure_sidecar(sock_path=sock_path) if lazy_start else (sock_path or socket_path())
    resp = _rpc_once(
        sock_path,
        {"op": "send_to_channel", "session_id": session_id, "envelope": envelope},
        timeout=timeout,
    )
    if not resp.get("ok"):
        raise MCPSidecarError(resp.get("reason", "unknown"))


def unregister(
    session_id: str,
    *,
    sock_path: Optional[Path] = None,
    timeout: float = DEFAULT_TIMEOUT,
) -> None:
    """Drop a registration (best-effort; missing rows are not an error)."""
    sock_path = sock_path or socket_path()
    try:
        _rpc_once(
            sock_path,
            {"op": "unregister_channel", "session_id": session_id},
            timeout=timeout,
        )
    except MCPSidecarUnreachable:
        # No sidecar => nothing to unregister.
        return
