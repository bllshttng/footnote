"""Grok's ACP-over-stdio driving lane.

The subprocess owns a held stdin pipe for the life of the session. ACP uses
JSON-RPC response ids for correlation, and notifications can arrive between a
request and its response, so a clean process exit is never a turn or request
receipt.
"""
from __future__ import annotations

import io
import json
import select
import subprocess
import threading
import time
from pathlib import Path
from typing import Any, Iterator, Optional, Sequence

from fno.agents.dispatch import DispatchAskError
from fno.agents.harnesses.pi import iter_jsonl

GROK_DEFAULT_MODEL = "grok-4.6"
GROK_DEFAULT_EFFORT = "high"
AUTH_MARKERS = ("not authenticated", "not signed in", "authentication required")

# A read that cannot time out is a hang, not a wait. ACP notifications arrive
# between a request and its response, so "no bytes yet" is normal and only a
# DEADLINE distinguishes a working turn from a dead one.
GROK_REQUEST_TIMEOUT_S = 180.0


def is_auth_error(text: str) -> bool:
    """One auth predicate, so every path agrees on what unauthenticated means.

    `require_authenticated` scanned these markers case-insensitively while
    `session_new` exact-matched a single message, so a grok that said "Not
    authenticated" raised a bare RuntimeError from one path and the typed
    exit-13 GrokAuthenticationRequired from the other.
    """
    lowered = text.lower()
    return any(marker in lowered for marker in AUTH_MARKERS)


def _error_detail(error: dict[str, Any], sep: str) -> str:
    """Join an ACP error's message and data so both reach the caller.

    `message` is a bare category ("Rate limited"); `data` carries the cause a
    caller can act on ("subscription:free-usage-exhausted"). Keeping only
    `message` reads a billing stop as an unexplained failure, which is the
    wrong thing to page someone about. One join for every raiser, so the two
    extraction sites cannot drift the way the auth predicate once did.
    """
    return sep.join(
        str(part) for part in (error.get("message", error), error.get("data")) if part
    )


class GrokAuthenticationRequired(DispatchAskError):
    """Grok needs an operator-owned login before it can create a session."""

    def __init__(self, detail: str = "") -> None:
        message = (
            "Grok authentication is required; run `grok login --device-code` "
            "or provide an operator-owned XAI_API_KEY."
        )
        if detail:
            message = f"{message} {detail}"
        super().__init__(message, exit_code=13)


def grok_stdio_argv(
    session_id: str,
    *,
    model: Optional[str] = None,
    effort: Optional[str] = None,
) -> list[str]:
    """Build Grok's ACP stdio argv.

    ``--session-id`` is passed for the CLI's own bookkeeping and is NOT the ACP
    session id. Measured 2026-08-30 against live grok 1.0.13: ``session/new``
    MINTS its own id and ignores this value, and ``session/list`` never shows
    it. Grok is callee-minted, like kimi, NOT caller-assigned like pi. Do not
    build an identity guarantee on this argument.
    """
    argv = [
        "grok",
        "--session-id",
        session_id,
        "--model",
        model or GROK_DEFAULT_MODEL,
        "--reasoning-effort",
        effort or GROK_DEFAULT_EFFORT,
        "agent",
        "stdio",
    ]
    return argv


def initialize_params() -> dict[str, Any]:
    """Return the standard ACP initialize request parameters."""
    return {
        "protocolVersion": 1,
        "clientInfo": {"name": "fno", "version": "0.1.0"},
        "clientCapabilities": {},
    }


def session_list_params(cwd: Path | str) -> dict[str, Any]:
    return {"cwd": str(cwd)}


def session_new_params(cwd: Path | str) -> dict[str, Any]:
    return {"cwd": str(cwd), "mcpServers": []}


def require_authenticated(
    completed: subprocess.CompletedProcess[str],
) -> str:
    """Reject a command that reports unauthenticated, even when it exits 0."""
    output = "\n".join(part for part in (completed.stdout, completed.stderr) if part)
    if is_auth_error(output):
        raise GrokAuthenticationRequired(output.strip())
    if completed.returncode:
        detail = output.strip() or f"exit status {completed.returncode}"
        raise DispatchAskError(f"grok command failed: {detail}", exit_code=completed.returncode)
    return completed.stdout


class GrokStdioSession:
    """A live ``grok agent stdio`` process with correlated ACP requests."""

    def __init__(
        self,
        session_id: str,
        cwd: Path | str,
        *,
        model: Optional[str] = None,
        effort: Optional[str] = None,
        argv: Optional[Sequence[str]] = None,
        env: Optional[dict[str, str]] = None,
    ) -> None:
        self.session_id = session_id
        self.cwd = Path(cwd)
        self.argv = list(
            argv or grok_stdio_argv(session_id, model=model, effort=effort)
        )
        self._env = env
        self.proc: Optional[subprocess.Popen[bytes]] = None
        self._events: Optional[Iterator[dict[str, Any]]] = None
        self.notifications: list[dict[str, Any]] = []
        self._stderr: list[str] = []
        self._stderr_thread: Optional[threading.Thread] = None
        self._request_id = 0
        self._read_deadline: Optional[float] = None

    def __enter__(self) -> "GrokStdioSession":
        self.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def start(self) -> "GrokStdioSession":
        self.proc = subprocess.Popen(
            self.argv,
            cwd=str(self.cwd),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=self._env,
        )
        stderr = self.proc.stderr
        if stderr is not None:
            def _drain() -> None:
                for line in iter(stderr.readline, b""):
                    self._stderr.append(line.decode("utf-8", "replace").rstrip("\n"))

            self._stderr_thread = threading.Thread(target=_drain, daemon=True)
            self._stderr_thread.start()
        return self

    @property
    def stderr_text(self) -> str:
        return "\n".join(self._stderr)

    def send(self, message: dict[str, Any]) -> None:
        if self.proc is None or self.proc.stdin is None:
            raise RuntimeError("grok ACP session is not started")
        try:
            self.proc.stdin.write((json.dumps(message) + "\n").encode())
            self.proc.stdin.flush()
        except BrokenPipeError as exc:
            # A dead child must fail typed: dispatch branches on exit codes,
            # and this is the one write path where a raw OSError would escape
            # that contract as an untyped traceback.
            code = self.proc.poll()
            raise DispatchAskError(
                f"grok ACP child is gone (broken pipe, code {code}); "
                f"stderr: {self.stderr_text or '<empty>'}",
                exit_code=code if code else 1,
            ) from exc

    def events(self) -> Iterator[dict[str, Any]]:
        if self.proc is None or self.proc.stdout is None:
            raise RuntimeError("grok ACP session is not started")
        if self._events is not None:
            return self._events
        stdout = self.proc.stdout
        if not isinstance(stdout, io.BufferedReader):
            # Popen(PIPE) yields a BufferedReader; a stream without read1
            # cannot be bounded, and an unbounded read is the defect this
            # module bans. Refuse the shape; never fall back to it.
            raise RuntimeError(
                f"grok ACP stdout is not a BufferedReader, so the read "
                f"cannot be bounded; stderr: {self.stderr_text or '<empty>'}"
            )

        def _chunks() -> Iterator[bytes]:
            while True:
                # Bounded by the per-request deadline `request()` publishes. An
                # unbounded read here hung initialize/session_new/prompt
                # forever whenever grok went silent without closing stdout.
                deadline = self._read_deadline
                if deadline is not None:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise self._read_timeout()
                    try:
                        ready, _, _ = select.select([stdout], [], [], remaining)
                    except (OSError, ValueError) as exc:
                        # A stream that cannot be watched cannot be bounded, and
                        # an unbounded read IS the defect. The first version of
                        # this guard fell back to `ready = [stdout]` here, which
                        # left the blocking read1 below reachable with the
                        # deadline already checked, so the hang survived on this
                        # one path while every path above it read as protected.
                        raise RuntimeError(
                            f"grok ACP stdout cannot be watched, so the read "
                            f"cannot be bounded: {exc}; "
                            f"stderr: {self.stderr_text or '<empty>'}"
                        ) from exc
                    if not ready:
                        raise self._read_timeout()
                chunk = stdout.read1(65536)
                if not chunk:
                    return
                yield chunk

        self._events = iter_jsonl(_chunks())
        return self._events

    def _read_timeout(self) -> RuntimeError:
        return RuntimeError(
            f"grok ACP read exceeded {GROK_REQUEST_TIMEOUT_S:.0f}s with no "
            f"response; stderr: {self.stderr_text or '<empty>'}"
        )

    def request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self._request_id += 1
        request_id = self._request_id
        self._read_deadline = time.monotonic() + GROK_REQUEST_TIMEOUT_S
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        for message in self.events():
            # A response has id and no method. A server-to-client request
            # (session/request_permission and friends) also carries an id, so
            # keying on the id alone either swallows it into notifications,
            # leaving grok blocked awaiting a permission until the deadline
            # reads as "read exceeded", or, on an id collision, returns the
            # server's request as the response to ours.
            if "method" in message and message.get("id") is not None:
                self.notifications.append(message)
                raise RuntimeError(
                    f"grok ACP sent server-to-client request "
                    f"{message.get('method')!r} (id {message.get('id')}) during "
                    f"{method!r}; fno answers no server requests yet; "
                    f"stderr: {self.stderr_text or '<empty>'}"
                )
            if message.get("id") == request_id:
                return message
            self.notifications.append(message)
        raise RuntimeError(
            f"grok ACP stream ended before response id {request_id} for {method!r}; "
            f"stderr: {self.stderr_text or '<empty>'}"
        )

    @staticmethod
    def result(response: dict[str, Any], method: str) -> dict[str, Any]:
        error = response.get("error")
        if isinstance(error, dict):
            detail = _error_detail(error, " ")
            raise RuntimeError(f"grok ACP {method} failed: {detail}")
        result = response.get("result")
        if not isinstance(result, dict):
            raise RuntimeError(f"grok ACP {method} returned no positive result")
        return result

    def initialize(self) -> dict[str, Any]:
        response = self.request("initialize", initialize_params())
        result = self.result(response, "initialize")
        if result.get("protocolVersion") != 1:
            raise RuntimeError(
                "grok ACP initialize did not return the positive protocolVersion 1 marker"
            )
        return result

    def session_list(self) -> dict[str, Any]:
        response = self.request("session/list", session_list_params(self.cwd))
        return self.result(response, "session/list")

    def session_new(self) -> str:
        response = self.request("session/new", session_new_params(self.cwd))
        error = response.get("error")
        if isinstance(error, dict):
            detail = _error_detail(error, " ")
            # Same predicate require_authenticated uses. The old exact match on
            # "Authentication required" dropped every other phrasing through to
            # result(), which raises a bare RuntimeError and loses exit 13.
            if is_auth_error(detail):
                if self.stderr_text:
                    detail = f"{detail}; stderr: {self.stderr_text}"
                raise GrokAuthenticationRequired(detail)
        result = self.result(response, "session/new")
        session_id = result.get("sessionId")
        if not isinstance(session_id, str) or not session_id:
            raise RuntimeError("grok ACP session/new returned no positive sessionId marker")
        self.session_id = session_id
        return session_id

    def prompt(self, message: str) -> dict[str, Any]:
        """Run one ACP turn and retain notifications for positive assertions."""
        response = self.request(
            "session/prompt",
            {
                "sessionId": self.session_id,
                "prompt": [{"type": "text", "text": message}],
            },
        )
        return self.result(response, "session/prompt")

    def close(self) -> None:
        if self.proc is None:
            return
        if self.proc.stdin is not None:
            try:
                self.proc.stdin.close()
            except OSError:
                pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=5)
        if self._stderr_thread is not None:
            self._stderr_thread.join(timeout=5)
            self._stderr_thread = None
        for stream in (self.proc.stdout, self.proc.stderr):
            if stream is not None:
                try:
                    stream.close()
                except OSError:
                    pass
        self._events = None
