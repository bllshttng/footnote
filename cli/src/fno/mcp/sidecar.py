"""fno MCP sidecar — per-user Unix-socket daemon.

Lifecycle owner: fno (per spec LD16). Lazy-start / lazy-exit (per LD3).
This is NOT the MCP server CC spawns — that is :mod:`channel_server`
(stdio child, CC-owned, per-session). The sidecar is the rendezvous
point so external ``fno agents ask`` processes can deliver pokes to a
particular Claude session: each ``channel_server`` registers its
session-id with the sidecar on startup, and the sidecar routes
inbound pokes back to the registered child over the same Unix-socket
connection that opened the registration.

Wire protocol (line-delimited JSON, request/response per call):

- ``{"op": "ping"}`` -> ``{"ok": True, "pid": <int>, "uptime_seconds": <int>}``
- ``{"op": "status"}`` -> ``{"ok": True, "channels": [...], "uptime_seconds": <int>}``
- ``{"op": "register_channel", "session_id": <str>, "channel_name": <str>, "pid": <int>}``
    On success the connection becomes a long-lived push channel; the
    sidecar writes ``{"op": "deliver", "envelope": {...}}`` lines down
    the socket whenever a poke arrives for this session.
- ``{"op": "unregister_channel", "session_id": <str>}`` -> ``{"ok": True}``
- ``{"op": "send_to_channel", "session_id": <str>, "envelope": {...}}``
    Routes the envelope to the registered channel's push connection.
    -> ``{"ok": True}`` on success, ``{"ok": False, "reason": "..."}``
    on miss.

Concurrency model:

- Single ``asyncio`` event loop.
- One task per accepted client connection.
- Channel registrations are tracked in an in-memory dict keyed by
  session id. A registered connection enters "push mode" — it stays
  open and the sidecar pushes deliver frames down it whenever a poke
  comes in for that session.
- Leader election is socket-bind exclusivity (per LD3): only one
  process can bind the socket file at a time.
- Lazy-exit timer is reset on every accepted connection AND every
  push delivery. Idle = no connections AND no registered channels AND
  no pending pokes for ``idle_exit_seconds``.

Stale-socket detection (per AC4-ERR + what-if F2):

On startup the sidecar first attempts a short-timeout connect to the
existing socket file. If the connect succeeds and a ``ping`` returns
``ok``, a prior sidecar is alive and this process exits 0 with stderr
"sidecar already running, pid=...". If the connect fails with
``ECONNREFUSED`` (socket file exists but no process listening), the
file is treated as a mid-bind-crash artifact: ``os.unlink`` it and
re-attempt ``bind()`` (bounded to 2 attempts).

Permissions: socket file is mode ``0600``, parent dir is mode ``0700``.
The sidecar refuses to bind under a world-readable parent.

This module is callable both as a library (``main_async`` /
``run_sidecar``) and as a script: ``python -m fno.mcp.sidecar``.
"""
from __future__ import annotations

import asyncio
import json
import logging
import os
import signal
import socket
import stat
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, Optional

LOG = logging.getLogger("fno.mcp.sidecar")

# Default idle-exit window (LD3 — overridable via settings.yaml or env).
DEFAULT_IDLE_EXIT_SECONDS = 30 * 60

# Wire-protocol error reasons (machine-stable).
_REASON_BAD_REQUEST = "bad_request"
_REASON_NOT_REGISTERED = "channel_not_registered"
_REASON_WRITE_FAILED = "channel_write_failed"
_REASON_ALREADY_REGISTERED = "already_registered"
_REASON_FOREIGN_UNREGISTER = "foreign_unregister_rejected"


def _socket_path() -> Path:
    """Resolve the sidecar socket path per spec AC3-XDG.

    Order: explicit ``$FNO_SIDECAR_SOCKET`` override (set by the
    client's lazy-start path when a non-default sock_path was requested,
    codex P2 review on PR #323), then ``$XDG_RUNTIME_DIR/fno/
    sidecar.sock`` (Linux desktop convention; manual on macOS), then
    ``<state_dir>/sidecar.sock`` via ``fno.paths.state_dir()`` so
    the path-config override surface (config.state_dir in settings.yaml)
    applies here too.
    """
    from fno import paths as _paths

    override = os.environ.get("FNO_SIDECAR_SOCKET", "").strip()
    if override:
        return Path(override)
    xdg = os.environ.get("XDG_RUNTIME_DIR", "").strip()
    if xdg:
        return Path(xdg) / "fno" / "sidecar.sock"
    return _paths.state_dir() / "sidecar.sock"


def _state_path() -> Path:
    """Persistent state file (registered channels at SIGTERM)."""
    from fno import paths as _paths

    return _paths.state_dir() / "sidecar-state.json"


def _ensure_parent_secure(p: Path) -> None:
    """Create the socket's parent directory at mode 0700.

    Refuses to operate under a world-readable parent because the socket
    inherits group/other access from the parent's traversal bits.
    """
    parent = p.parent
    parent.mkdir(parents=True, exist_ok=True)
    try:
        st = parent.stat()
        # Tighten permissions if currently looser than 0700 (mode bits).
        if stat.S_IMODE(st.st_mode) & 0o077:
            parent.chmod(0o700)
    except OSError as exc:  # pragma: no cover - best-effort
        LOG.warning("could not tighten parent dir %s: %s", parent, exc)


@dataclass
class _Channel:
    """In-memory registration record for one channel server child."""

    session_id: str
    channel_name: str
    pid: int
    writer: asyncio.StreamWriter
    registered_at: float = field(default_factory=time.time)


class SidecarState:
    """Mutable state for one sidecar process."""

    def __init__(self, *, idle_exit_seconds: int = DEFAULT_IDLE_EXIT_SECONDS) -> None:
        self.channels: Dict[str, _Channel] = {}
        self.started_at: float = time.time()
        self.last_activity_at: float = time.time()
        self.idle_exit_seconds: int = idle_exit_seconds

    def touch(self) -> None:
        self.last_activity_at = time.time()

    def uptime_seconds(self) -> int:
        return int(time.time() - self.started_at)

    def is_idle(self) -> bool:
        """``True`` when no channels are registered AND idle window
        elapsed.

        Per LD3: sidecar does NOT idle-out while channel servers are
        checked in (LD3 phrasing: "Idle means no registered channel
        servers AND no pending pokes for > idle_exit_seconds").
        """
        if self.channels:
            return False
        return (time.time() - self.last_activity_at) >= self.idle_exit_seconds


# ---------------------------------------------------------------------
# Protocol helpers
# ---------------------------------------------------------------------


async def _write_line(writer: asyncio.StreamWriter, payload: Dict[str, Any]) -> bool:
    """Serialize ``payload`` as a single JSON line and flush.

    Returns ``True`` on success, ``False`` if the peer closed the
    connection mid-write.
    """
    try:
        line = json.dumps(payload, ensure_ascii=False) + "\n"
        writer.write(line.encode("utf-8"))
        await writer.drain()
        return True
    except (BrokenPipeError, ConnectionResetError, OSError):
        return False


async def _read_line(reader: asyncio.StreamReader) -> Optional[Dict[str, Any]]:
    """Read one JSON line; return ``None`` on EOF or parse error."""
    try:
        raw = await reader.readline()
    except (ConnectionResetError, OSError):
        return None
    if not raw:
        return None
    try:
        parsed: Dict[str, Any] = json.loads(raw.decode("utf-8"))
        return parsed
    except (json.JSONDecodeError, UnicodeDecodeError):
        return None


# ---------------------------------------------------------------------
# Per-connection handler
# ---------------------------------------------------------------------


async def _handle_client(
    reader: asyncio.StreamReader,
    writer: asyncio.StreamWriter,
    state: SidecarState,
) -> None:
    """Serve one client connection until EOF or unregister.

    A connection that issues ``register_channel`` becomes long-lived;
    the writer is retained in ``state.channels`` so the sidecar can
    push ``deliver`` frames down it whenever a poke arrives for the
    registered session. All other ops are single-shot.
    """
    state.touch()
    # All session_ids this connection has registered. The protocol allows
    # multiple register_channel calls on one connection; the cleanup path
    # below MUST purge every one of them on disconnect or stale entries
    # linger until a subsequent send_to_channel happens to fail (Gemini
    # #2, PR #323). Using a set + dict-membership test in the finally
    # block handles the multi-register case correctly.
    registered_session_ids: set[str] = set()

    try:
        while True:
            req = await _read_line(reader)
            if req is None:
                # Peer closed the connection — drop the registration
                # if this was a push channel.
                break
            state.touch()
            op = req.get("op")
            if op == "ping":
                await _write_line(
                    writer,
                    {"ok": True, "pid": os.getpid(), "uptime_seconds": state.uptime_seconds()},
                )
            elif op == "status":
                await _write_line(
                    writer,
                    {
                        "ok": True,
                        "pid": os.getpid(),
                        "uptime_seconds": state.uptime_seconds(),
                        "channels_registered": len(state.channels),
                        "channels": [
                            {
                                "session_id": ch.session_id,
                                "channel_name": ch.channel_name,
                                "pid": ch.pid,
                                "registered_seconds_ago": int(time.time() - ch.registered_at),
                            }
                            for ch in state.channels.values()
                        ],
                    },
                )
            elif op == "register_channel":
                sid = req.get("session_id")
                cname = req.get("channel_name")
                pid = req.get("pid")
                if not (isinstance(sid, str) and isinstance(cname, str) and isinstance(pid, int)):
                    await _write_line(
                        writer, {"ok": False, "reason": _REASON_BAD_REQUEST}
                    )
                    continue
                existing_ch = state.channels.get(sid)
                if existing_ch is not None and existing_ch.writer is not writer:
                    # Spec invariant: registration is idempotent per
                    # connection, but a DIFFERENT connection trying to
                    # claim the same session_id is a corruption signal -
                    # reject and log so reconcile can flag the entry.
                    LOG.warning(
                        "rejected duplicate register_channel from foreign "
                        "connection: session=%s",
                        sid,
                    )
                    await _write_line(
                        writer,
                        {"ok": False, "reason": _REASON_ALREADY_REGISTERED},
                    )
                    continue
                state.channels[sid] = _Channel(
                    session_id=sid,
                    channel_name=cname,
                    pid=pid,
                    writer=writer,
                )
                registered_session_ids.add(sid)
                LOG.info("registered channel: session=%s name=%s pid=%s", sid, cname, pid)
                await _write_line(writer, {"ok": True})
                # NOTE: do NOT break here; the connection stays open
                # for the sidecar to push deliver frames down it.
            elif op == "unregister_channel":
                sid = req.get("session_id")
                if isinstance(sid, str) and sid in state.channels:
                    # Only the owning connection (the one that registered
                    # this session_id) may unregister it. Otherwise any
                    # client over the per-user socket could evict any
                    # registration. Rejection here is silent in the
                    # response shape (ok: false) because per-user socket
                    # access is already a trust boundary; the log entry
                    # is the operator-visible signal.
                    if state.channels[sid].writer is not writer:
                        LOG.warning(
                            "rejected unregister_channel from foreign "
                            "connection: session=%s",
                            sid,
                        )
                        await _write_line(
                            writer,
                            {"ok": False, "reason": _REASON_FOREIGN_UNREGISTER},
                        )
                        continue
                    del state.channels[sid]
                    LOG.info("unregistered channel: session=%s", sid)
                if isinstance(sid, str):
                    registered_session_ids.discard(sid)
                await _write_line(writer, {"ok": True})
            elif op == "send_to_channel":
                sid = req.get("session_id")
                envelope = req.get("envelope")
                if not (isinstance(sid, str) and isinstance(envelope, dict)):
                    await _write_line(
                        writer, {"ok": False, "reason": _REASON_BAD_REQUEST}
                    )
                    continue
                ch = state.channels.get(sid)
                if ch is None:
                    await _write_line(
                        writer, {"ok": False, "reason": _REASON_NOT_REGISTERED}
                    )
                    continue
                ok = await _write_line(
                    ch.writer, {"op": "deliver", "envelope": envelope}
                )
                if not ok:
                    # The channel server's connection went down — purge
                    # the registration. The next probe will report it as
                    # unreachable; reconcile flips status to orphaned.
                    LOG.warning(
                        "channel write failed; purging session=%s", sid
                    )
                    state.channels.pop(sid, None)
                    await _write_line(
                        writer, {"ok": False, "reason": _REASON_WRITE_FAILED}
                    )
                else:
                    await _write_line(writer, {"ok": True})
            else:
                await _write_line(
                    writer, {"ok": False, "reason": _REASON_BAD_REQUEST}
                )
    except asyncio.CancelledError:  # pragma: no cover - shutdown path
        raise
    except Exception as exc:  # pragma: no cover - defense in depth
        LOG.exception("client handler crashed: %s", exc)
    finally:
        # Purge EVERY session_id this connection registered. The
        # protocol allows multiple register_channel calls on one
        # connection; the prior single-id cleanup left stale entries
        # for the second+ registrations (Gemini #2, PR #323).
        for sid in list(registered_session_ids):
            ch = state.channels.get(sid)
            if ch is not None and ch.writer is writer:
                del state.channels[sid]
                LOG.info(
                    "purged stale registration on client close: session=%s",
                    sid,
                )
        try:
            writer.close()
        except Exception:  # pragma: no cover
            pass


# ---------------------------------------------------------------------
# Lazy-exit + signal handlers
# ---------------------------------------------------------------------


async def _idle_watcher(state: SidecarState, stop_event: asyncio.Event) -> None:
    """Background task that exits the sidecar when idle.

    Polls every ``min(60, idle_exit_seconds // 2)`` seconds. The event
    loop terminates when ``stop_event`` is set.
    """
    interval = max(5, min(60, state.idle_exit_seconds // 2))
    while not stop_event.is_set():
        try:
            await asyncio.wait_for(stop_event.wait(), timeout=interval)
            return
        except asyncio.TimeoutError:
            if state.is_idle():
                LOG.info(
                    "sidecar idle for %ss with %s channels; exiting",
                    int(time.time() - state.last_activity_at),
                    len(state.channels),
                )
                stop_event.set()
                return


def _install_signal_handlers(
    loop: asyncio.AbstractEventLoop,
    state: SidecarState,
    stop_event: asyncio.Event,
) -> None:
    """Wire SIGTERM/SIGINT to flush state and signal stop."""

    def _on_signal(signame: str) -> None:
        LOG.info("sidecar received %s; flushing state and shutting down", signame)
        try:
            _flush_state(state)
        except Exception as exc:  # pragma: no cover
            LOG.warning("flush_state failed: %s", exc)
        stop_event.set()

    for sig_name, sig in (("SIGTERM", signal.SIGTERM), ("SIGINT", signal.SIGINT)):
        try:
            loop.add_signal_handler(sig, _on_signal, sig_name)
        except (NotImplementedError, RuntimeError):  # pragma: no cover
            # Windows doesn't support add_signal_handler — fall back.
            signal.signal(sig, lambda *_: _on_signal(sig_name))


def _flush_state(state: SidecarState) -> None:
    """Persist the channel-registration table to disk.

    SIGKILL leaves this file stale, which is fine: channel servers
    re-register on next session boot, and the registry's
    ``mcp_channel_id`` field is the persistent source of truth.
    """
    path = _state_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "pid": os.getpid(),
        "flushed_at": int(time.time()),
        "channels": [
            {
                "session_id": ch.session_id,
                "channel_name": ch.channel_name,
                "pid": ch.pid,
            }
            for ch in state.channels.values()
        ],
    }
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(payload, indent=2), encoding="utf-8")
    os.replace(tmp, path)


# ---------------------------------------------------------------------
# Bind + lazy-start support
# ---------------------------------------------------------------------


def _is_sidecar_running(sock_path: Path, *, timeout: float = 0.5) -> bool:
    """Probe an existing sidecar via a short-timeout connect + ping.

    Returns ``True`` only if the connect succeeds AND the peer responds
    to a ``ping`` within ``timeout`` seconds. Anything else (no socket
    file, ECONNREFUSED, no response) returns ``False``; the caller is
    free to unlink the stale file and re-bind.
    """
    if not sock_path.exists():
        return False
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    try:
        s.connect(str(sock_path))
        s.sendall((json.dumps({"op": "ping"}) + "\n").encode("utf-8"))
        data = s.recv(4096)
        if not data:
            return False
        try:
            resp = json.loads(data.decode("utf-8").splitlines()[0])
        except (json.JSONDecodeError, IndexError, UnicodeDecodeError):
            return False
        return bool(resp.get("ok"))
    except (ConnectionRefusedError, socket.timeout, OSError):
        return False
    finally:
        try:
            s.close()
        except OSError:  # pragma: no cover
            pass


def _prepare_socket_path(sock_path: Path, *, max_attempts: int = 2) -> Optional[str]:
    """Ensure ``sock_path`` is bindable.

    Asyncio's ``start_unix_server`` performs the actual ``bind()`` call;
    this helper's job is the pre-bind probe + stale-socket recovery so
    the real bind sees a clean path. AC3-MIDBIND case: if the socket
    file exists but a connect attempt fails with ECONNREFUSED, the
    file is a stale-from-mid-bind-crash artifact — unlink it and
    re-attempt the probe (bounded to ``max_attempts`` so a tight
    re-bind-crash race doesn't loop forever).

    Returns ``None`` when the path is ready for a fresh bind,
    ``"already_running"`` if a live sidecar holds the socket, or
    a short ``"<kind>:<errno>"`` reason on unrecoverable failure.
    """
    _ensure_parent_secure(sock_path)
    last_err: Optional[str] = None
    for _ in range(max_attempts):
        if not sock_path.exists():
            return None
        if _is_sidecar_running(sock_path, timeout=0.3):
            return "already_running"
        try:
            sock_path.unlink()
        except FileNotFoundError:
            # Lost the race against another unlink — that's fine,
            # the path is now free.
            return None
        except OSError as exc:
            last_err = f"unlink_failed:{exc.errno}"
            continue
    return last_err or "stale_socket_unlink_exhausted"


# Backwards-compatible alias — older callers expected ``_bind_socket``.
_bind_socket = _prepare_socket_path


# ---------------------------------------------------------------------
# Main loop
# ---------------------------------------------------------------------


async def main_async(
    *,
    sock_path: Optional[Path] = None,
    idle_exit_seconds: int = DEFAULT_IDLE_EXIT_SECONDS,
) -> int:
    """Run the sidecar event loop.

    Returns an exit code suitable for ``sys.exit``.
    """
    sock_path = sock_path or _socket_path()
    prep_status = _prepare_socket_path(sock_path)
    if prep_status == "already_running":
        print(f"sidecar already running at {sock_path}", file=sys.stderr)
        return 0
    if prep_status is not None:
        print(f"sidecar bind preparation failed: {prep_status}", file=sys.stderr)
        return 2

    state = SidecarState(idle_exit_seconds=idle_exit_seconds)
    stop_event = asyncio.Event()
    loop = asyncio.get_running_loop()
    _install_signal_handlers(loop, state, stop_event)

    # AC3-MIDBIND defense: the real bind happens in start_unix_server.
    # If a third party re-created the stale socket between the
    # _prepare_socket_path probe and start_unix_server's bind, we get
    # an OSError. Re-prepare once and retry. Bounded to two attempts
    # so a tight stale-re-create loop fails loudly instead of spinning.
    async def _bind_with_retry() -> asyncio.base_events.Server:
        for attempt in range(2):
            try:
                return await asyncio.start_unix_server(
                    lambda r, w: _handle_client(r, w, state),
                    path=str(sock_path),
                )
            except OSError as exc:
                if attempt == 1:
                    raise
                # Stale-socket re-create race: retry after re-prepare.
                LOG.warning(
                    "start_unix_server bind raced; re-preparing path (errno=%s)",
                    exc.errno,
                )
                prep = _prepare_socket_path(sock_path)
                if prep == "already_running":
                    raise OSError(
                        f"sidecar lost bind race to a live peer at {sock_path}"
                    )
                if prep is not None:
                    raise OSError(f"re-prepare failed: {prep}")
        # Unreachable; placate type checker.
        raise OSError("bind retry exhausted")

    # Restrict the process umask BEFORE start_unix_server creates the
    # socket file so the file is born at mode 0600 — without this, the
    # process's default umask (typically 022) leaves the socket
    # world-readable for the window between bind() and our follow-up
    # chmod (Gemini #3, PR #323). The chmod below remains as a belt-
    # and-suspenders for the rare case where the umask change is
    # ineffective (e.g. acl-protected filesystem).
    _prior_umask = os.umask(0o077)
    try:
        server = await _bind_with_retry()
    except OSError as exc:
        os.umask(_prior_umask)
        print(
            f"sidecar start_unix_server failed (errno={exc.errno}): {exc}",
            file=sys.stderr,
        )
        return 2
    os.umask(_prior_umask)
    try:
        sock_path.chmod(0o600)
    except OSError as exc:  # pragma: no cover - permission only matters when
        LOG.warning("could not chmod socket to 0600: %s", exc)

    LOG.info(
        "sidecar listening at %s pid=%s idle_exit_seconds=%s",
        sock_path,
        os.getpid(),
        idle_exit_seconds,
    )

    idle_task = asyncio.create_task(_idle_watcher(state, stop_event))
    try:
        await stop_event.wait()
    finally:
        idle_task.cancel()
        server.close()
        try:
            await server.wait_closed()
        except Exception:  # pragma: no cover
            pass
        try:
            if sock_path.exists():
                sock_path.unlink()
        except OSError:  # pragma: no cover
            pass

    return 0


def run_sidecar(
    *,
    sock_path: Optional[Path] = None,
    idle_exit_seconds: int = DEFAULT_IDLE_EXIT_SECONDS,
) -> int:
    """Synchronous entry point. Configures logging and runs the loop."""
    level = os.environ.get("FNO_MCP_SIDECAR_LOG", "INFO").upper()
    logging.basicConfig(
        level=getattr(logging, level, logging.INFO),
        format="[sidecar %(asctime)s %(levelname)s] %(message)s",
    )
    try:
        return asyncio.run(
            main_async(sock_path=sock_path, idle_exit_seconds=idle_exit_seconds)
        )
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":  # pragma: no cover - script entry
    sys.exit(run_sidecar())
