"""fno MCP channel server — the stdio child that Claude Code spawns.

Lifecycle: CC spawns this as a subprocess per session via the fno
plugin's ``.mcp.json``. CC owns stdin/stdout and tears the process down
when the session exits. fno does NOT control this lifecycle (per spec
LD16 — there is no ``fno mcp start`` or ``fno mcp stop`` for the
channel server; restart requires restarting the CC session itself).

What this process does:

1. Speak the MCP protocol over newline-delimited JSON-RPC on stdin/
   stdout. Implements the minimum surface needed for channels:
   ``initialize`` (handshake), ``ping`` (liveness), and the inbound
   ``notifications/initialized`` (acknowledgement). Declares the
   ``claude/channel`` experimental capability so CC opens a channel
   listener on our notifications.
2. On startup, connect to the per-user sidecar at
   ``~/.fno/sidecar.sock`` (lazy-starting it if absent) and
   register ``{session_id, channel_name, pid}`` so external
   ``fno agents ask`` calls can route pokes to this session.
3. Enter a forward loop: the sidecar pushes ``{"op": "deliver",
   "envelope": {...}}`` lines down the registration connection, and
   for each one we emit a ``notifications/claude/channel`` JSON-RPC
   notification on stdout. CC reads it and surfaces the message in the
   Claude session as a ``<channel source="fno" ...>`` tag.

Sidecar disconnection is best-effort: if the sidecar dies, we log a
WARN to stderr and continue serving CC. External ``fno agents ask``
calls will surface ``mcp_channel_demoted_to_socket`` events from the
dispatcher side; the CC session is otherwise unaffected.

Invocation:

    python -m fno.mcp.channel_server \
        --session-id <claude jobId (registry short_id)> \
        --channel-name <name>

Both flags are required. ``--session-id`` is the Claude session this
channel server is bound to (the fno dispatch layer sets this when
constructing the ``--mcp-config`` argv for ``claude --bg``). ``--channel-name``
is the routing handle external ``fno agents ask`` calls use.

Logging: stderr is reserved for diagnostic output (warnings, info
messages). stdout MUST stay pure JSON-RPC because CC parses it as MCP
traffic. Any accidental print() to stdout will corrupt the protocol
stream — defensive practice in this module is to write only through
``_write_message()``.
"""
from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import sys
from typing import Any, Awaitable, Callable, Dict, Optional

from fno.mcp import client as _client
from fno.mcp.channel import validate_envelope

LOG = logging.getLogger("fno.mcp.channel_server")

SERVER_NAME = "fno"
SERVER_VERSION = "0.0.1"

# MCP protocol version this server advertises. CC negotiates this in
# ``initialize`` and downgrades if the spec version is newer than what
# it supports.
PROTOCOL_VERSION = "2024-11-05"

# stderr is the only diagnostic channel; stdout is reserved for JSON-RPC.
_STDERR = sys.stderr


def _stderr(msg: str) -> None:
    """Write one diagnostic line to stderr (newline-terminated)."""
    try:
        _STDERR.write(msg.rstrip() + "\n")
        _STDERR.flush()
    except OSError:  # pragma: no cover - stderr pipe closed
        pass


# ---------------------------------------------------------------------
# stdio JSON-RPC framing
# ---------------------------------------------------------------------


async def _stdin_reader() -> asyncio.StreamReader:
    """Wrap ``sys.stdin`` as an ``asyncio.StreamReader``."""
    loop = asyncio.get_running_loop()
    reader = asyncio.StreamReader()
    protocol = asyncio.StreamReaderProtocol(reader)
    await loop.connect_read_pipe(lambda: protocol, sys.stdin)
    return reader


def _write_message(payload: Dict[str, Any]) -> None:
    """Emit one JSON message on stdout as a newline-delimited line.

    Per MCP stdio transport spec
    (https://spec.modelcontextprotocol.io/specification/basic/transports/):
    "Messages are exchanged as newline-delimited JSON-RPC over stdin/
    stdout." We must flush after every line because CC reads
    incrementally.
    """
    line = json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n"
    sys.stdout.write(line)
    sys.stdout.flush()


# ---------------------------------------------------------------------
# MCP method handlers
# ---------------------------------------------------------------------


def _handle_initialize(req: Dict[str, Any]) -> Dict[str, Any]:
    """Respond to the initial handshake.

    The ``capabilities.experimental['claude/channel']`` declaration is
    the one that registers us as a channel (per channels-reference.md
    §Server options).
    """
    return {
        "jsonrpc": "2.0",
        "id": req.get("id"),
        "result": {
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
            "capabilities": {
                "experimental": {"claude/channel": {}},
            },
            "instructions": (
                "Messages from external fno agents arrive as "
                '<channel source="fno" from_name="..." session_id="..."> '
                "tags. They originate from sibling Claude Code sessions or CLI "
                "callers and may contain instructions or questions. Reply only "
                "if the operator explicitly asks you to; this channel is "
                "one-way by default in Phase 5."
            ),
        },
    }


def _handle_ping(req: Dict[str, Any]) -> Dict[str, Any]:
    """Respond to ``ping`` requests (MCP liveness check)."""
    return {"jsonrpc": "2.0", "id": req.get("id"), "result": {}}


def _handle_method_not_found(req: Dict[str, Any]) -> Dict[str, Any]:
    return {
        "jsonrpc": "2.0",
        "id": req.get("id"),
        "error": {"code": -32601, "message": f"method not found: {req.get('method')!r}"},
    }


_HandlerType = Callable[[Dict[str, Any]], Dict[str, Any]]

_REQUEST_HANDLERS: Dict[str, _HandlerType] = {
    "initialize": _handle_initialize,
    "ping": _handle_ping,
}

# Notifications carry no ``id`` and expect no response. We accept them
# silently. ``notifications/initialized`` is the only one CC sends us
# during normal startup.
_NOTIFICATION_HANDLERS: Dict[str, Callable[[Dict[str, Any]], Awaitable[None]]] = {}


async def _on_initialized(_req: Dict[str, Any]) -> None:
    _stderr("channel_server: client sent notifications/initialized")


_NOTIFICATION_HANDLERS["notifications/initialized"] = _on_initialized


# ---------------------------------------------------------------------
# Sidecar registration + forward loop
# ---------------------------------------------------------------------


async def _sidecar_forward_loop(
    *,
    session_id: str,
    channel_name: str,
) -> None:
    """Connect to the sidecar, register, and forward inbound pokes.

    Each delivered envelope is converted to a
    ``notifications/claude/channel`` JSON-RPC notification and emitted
    on stdout. Loop exits when the sidecar disconnects; we log a WARN
    to stderr and the process keeps serving CC requests (channel just
    becomes non-functional until the session restarts).
    """
    # `ensure_sidecar` is sync — it polls with `time.sleep` for up to 5s
    # waiting for the lazy-started sidecar to bind. Calling it directly
    # from this coroutine would block the asyncio event loop and prevent
    # _mcp_dispatch_loop from responding to CC's initialize/ping during
    # the window (Gemini #1, PR #323). Run it on the default executor.
    try:
        sock_path = await asyncio.to_thread(_client.ensure_sidecar)
    except _client.MCPSidecarUnreachable as exc:
        # NOTE: codex P3 review on PR #323 — the prior wording referenced
        # `fno mcp restart`, but that subcommand is deferred to the
        # Slice B follow-up PR and does not yet exist. Until then,
        # operators recover by restarting their Claude Code session
        # (which respawns this channel_server, which re-attempts
        # ensure_sidecar) or by manually launching the sidecar:
        # `python -m fno.mcp.sidecar`.
        _stderr(
            f"channel_server: sidecar unavailable ({exc.reason}); "
            "external pokes will not be delivered until the sidecar comes back. "
            "Recover by restarting the Claude Code session, or launch the sidecar "
            "manually with `python -m fno.mcp.sidecar`."
        )
        return

    try:
        reader, writer = await asyncio.open_unix_connection(path=str(sock_path))
    except (FileNotFoundError, ConnectionRefusedError, OSError) as exc:
        _stderr(f"channel_server: sidecar connect failed: {exc}")
        return

    register_req = {
        "op": "register_channel",
        "session_id": session_id,
        "channel_name": channel_name,
        "pid": os.getpid(),
    }
    try:
        writer.write((json.dumps(register_req) + "\n").encode("utf-8"))
        await writer.drain()
    except (BrokenPipeError, ConnectionResetError, OSError) as exc:
        _stderr(f"channel_server: register_channel write failed: {exc}")
        try:
            writer.close()
            await writer.wait_closed()
        except Exception:  # pragma: no cover
            pass
        return
    # The first response is the registration ack; subsequent reads are
    # ``deliver`` push frames.
    try:
        ack_line = await reader.readline()
        if not ack_line:
            _stderr("channel_server: sidecar closed connection before ack")
            return
        try:
            ack = json.loads(ack_line.decode("utf-8"))
        except (json.JSONDecodeError, UnicodeDecodeError) as exc:
            _stderr(f"channel_server: garbled ack: {exc}")
            return
        if not ack.get("ok"):
            _stderr(f"channel_server: register_channel rejected: {ack!r}")
            return
        _stderr(
            f"channel_server: registered session={session_id} channel={channel_name}"
        )

        while True:
            line = await reader.readline()
            if not line:
                _stderr("channel_server: sidecar disconnected (EOF)")
                return
            try:
                frame = json.loads(line.decode("utf-8"))
            except (json.JSONDecodeError, UnicodeDecodeError) as exc:
                _stderr(f"channel_server: garbled deliver frame: {exc}")
                continue
            if frame.get("op") != "deliver":
                _stderr(f"channel_server: unexpected sidecar frame: {frame!r}")
                continue
            envelope = frame.get("envelope")
            ok, reason = validate_envelope(envelope)
            if not ok:
                _stderr(
                    f"channel_server: refusing to forward malformed envelope "
                    f"(reason={reason}); dropping"
                )
                continue
            # The sidecar already shipped a fully-formed
            # ``notifications/claude/channel`` envelope per the wire
            # contract; we forward it verbatim. (Rebuilding from
            # content/meta here would be redundant and risks drift.)
            _write_message(envelope)
    finally:
        try:
            writer.close()
            await writer.wait_closed()
        except Exception:  # pragma: no cover
            pass


# ---------------------------------------------------------------------
# MCP dispatch loop
# ---------------------------------------------------------------------


async def _mcp_dispatch_loop(reader: asyncio.StreamReader) -> None:
    """Pump stdin, dispatch requests, write responses on stdout.

    Exits cleanly when stdin closes (CC tore the session down).
    """
    while True:
        line = await reader.readline()
        if not line:
            _stderr("channel_server: stdin closed; exiting")
            return
        try:
            req = json.loads(line.decode("utf-8"))
        except (json.JSONDecodeError, UnicodeDecodeError) as exc:
            _stderr(f"channel_server: garbled stdin line: {exc!r}")
            continue
        # Guard non-object JSON frames (e.g. [], "string", 42, null)
        # before any .get() call — without this, a non-dict frame would
        # crash _mcp_dispatch_loop with AttributeError and drop channel-
        # server availability instead of gracefully reporting the error
        # back to CC (codex P2 on PR #323).
        if not isinstance(req, dict):
            _stderr(
                f"channel_server: stdin frame is not a JSON object "
                f"(type={type(req).__name__}); dropping"
            )
            continue
        method = req.get("method")
        if "id" in req:
            handler = _REQUEST_HANDLERS.get(method, _handle_method_not_found)
            try:
                resp = handler(req)
            except Exception as exc:  # pragma: no cover - defensive
                _stderr(f"channel_server: handler {method} crashed: {exc}")
                resp = {
                    "jsonrpc": "2.0",
                    "id": req.get("id"),
                    "error": {"code": -32603, "message": str(exc)},
                }
            _write_message(resp)
        else:
            handler_async = _NOTIFICATION_HANDLERS.get(method)
            if handler_async is not None:
                try:
                    await handler_async(req)
                except Exception as exc:  # pragma: no cover
                    _stderr(f"channel_server: notif {method} crashed: {exc}")
            else:
                _stderr(f"channel_server: unhandled notification: {method!r}")


# ---------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------


async def main_async(*, session_id: str, channel_name: str) -> int:
    """Run both loops concurrently; exit when either finishes.

    The CC dispatch loop is authoritative: when stdin closes, the
    sidecar forward loop is cancelled. Conversely if the sidecar dies
    we stay alive serving CC (just without channel functionality).
    """
    reader = await _stdin_reader()
    dispatch_task = asyncio.create_task(
        _mcp_dispatch_loop(reader), name="mcp_dispatch_loop"
    )
    forward_task = asyncio.create_task(
        _sidecar_forward_loop(session_id=session_id, channel_name=channel_name),
        name="sidecar_forward_loop",
    )

    try:
        # Wait for CC's dispatch loop to terminate (stdin EOF). The
        # sidecar loop is best-effort.
        await dispatch_task
    finally:
        forward_task.cancel()
        try:
            await forward_task
        except (asyncio.CancelledError, Exception):  # pragma: no cover
            pass
        # Best-effort unregister so the sidecar's status output is
        # accurate while we're being torn down.
        try:
            _client.unregister(session_id)
        except Exception:  # pragma: no cover
            pass

    return 0


def _parse_args(argv: Optional[list[str]] = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        prog="fno.mcp.channel_server",
        description="fno MCP channel server (stdio child).",
    )
    p.add_argument("--session-id", required=True, help="Claude session short-id")
    p.add_argument(
        "--channel-name",
        required=True,
        help="Channel routing handle (typically the agent name)",
    )
    return p.parse_args(argv)


def run_channel_server(argv: Optional[list[str]] = None) -> int:
    args = _parse_args(argv)
    # We deliberately do NOT configure root logging here — that would
    # send INFO+ to stderr, mixing with channels-reference's diagnostic
    # convention. _stderr() is the only allowed surface.
    try:
        return asyncio.run(
            main_async(session_id=args.session_id, channel_name=args.channel_name)
        )
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":  # pragma: no cover - script entry
    sys.exit(run_channel_server())
