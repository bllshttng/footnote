"""The shared ACP-over-stdio session core for the grok and kimi drivers.

Two harnesses speak the same Agent Client Protocol over a held stdin pipe, so
one protocol loop lives here: bounded reads, correlated request ids, the
server-to-client refusal, and the typed broken pipe. A fix to the core cannot
drift away from the other driver. The drivers keep their identity layer:
argv, auth vocabulary, positive markers, per-harness params and timeouts.

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


def error_detail(error: dict[str, Any], sep: str = " ") -> str:
    """Join an ACP error's message and data so both reach the caller.

    `message` is a bare category ("Rate limited"); `data` carries the cause a
    caller can act on ("subscription:free-usage-exhausted"). Keeping only
    `message` reads a billing stop as an unexplained failure. One join for
    every raiser, so the extraction sites cannot drift.
    """
    return sep.join(
        str(part) for part in (error.get("message", error), error.get("data")) if part
    )


class AcpStdioSession:
    """A live ACP process with correlated requests over a held stdin pipe.

    Subclasses name the tool and answer two hooks: ``_request_timeout`` for
    the bounded-read deadline (resolved at call time, so tests can shorten it
    through the driver module's constant) and, where a driver needs one,
    ``_session_list_params`` for its own session/list shape.
    """

    tool = "acp"

    def __init__(self, cwd: Path | str, *, argv: Sequence[str], env: Optional[dict[str, str]] = None) -> None:
        self.cwd = Path(cwd)
        self.argv = list(argv)
        self.session_id: Optional[str] = None
        self._env = env
        self.proc: Optional[subprocess.Popen[bytes]] = None
        self._events: Optional[Iterator[dict[str, Any]]] = None
        self.notifications: list[dict[str, Any]] = []
        self._stderr: list[str] = []
        self._stderr_thread: Optional[threading.Thread] = None
        self._request_id = 0
        self._read_deadline: Optional[float] = None

    def _request_timeout(self) -> float:
        raise NotImplementedError("the driver names its own bounded-read deadline")

    def _session_list_params(self) -> dict[str, Any]:
        return {"cwd": str(self.cwd)}

    def __enter__(self) -> "AcpStdioSession":
        self.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def start(self) -> "AcpStdioSession":
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
            raise RuntimeError(f"{self.tool} ACP session is not started")
        try:
            self.proc.stdin.write((json.dumps(message) + "\n").encode())
            self.proc.stdin.flush()
        except BrokenPipeError as exc:
            # A dead child must fail typed: dispatch branches on exit codes,
            # and this is the one write path where a raw OSError would escape
            # that contract as an untyped traceback.
            code = self.proc.poll()
            raise DispatchAskError(
                f"{self.tool} ACP child is gone (broken pipe, code {code}); "
                f"stderr: {self.stderr_text or '<empty>'}",
                exit_code=code if code else 1,
            ) from exc

    def events(self) -> Iterator[dict[str, Any]]:
        if self.proc is None or self.proc.stdout is None:
            raise RuntimeError(f"{self.tool} ACP session is not started")
        if self._events is not None:
            return self._events
        stdout = self.proc.stdout
        if not isinstance(stdout, io.BufferedReader):
            # Popen(PIPE) yields a BufferedReader; a stream without read1
            # cannot be bounded, and an unbounded read is the defect this
            # module bans. Refuse the shape; never fall back to it.
            raise RuntimeError(
                f"{self.tool} ACP stdout is not a BufferedReader, so the read "
                f"cannot be bounded; stderr: {self.stderr_text or '<empty>'}"
            )

        def _chunks() -> Iterator[bytes]:
            while True:
                # Bounded by the per-request deadline `request()` publishes. An
                # unbounded read here hangs initialize/session_new/prompt
                # forever whenever the harness went silent without closing
                # stdout.
                deadline = self._read_deadline
                if deadline is not None:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise self._read_timeout()
                    try:
                        ready, _, _ = select.select([stdout], [], [], remaining)
                    except (OSError, ValueError) as exc:
                        # A stream that cannot be watched cannot be bounded, and
                        # an unbounded read IS the defect. Refuse instead of
                        # falling back to the blocking read1 below.
                        raise RuntimeError(
                            f"{self.tool} ACP stdout cannot be watched, so the "
                            f"read cannot be bounded: {exc}; "
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
            f"{self.tool} ACP read exceeded {self._request_timeout():.0f}s with no "
            f"response; stderr: {self.stderr_text or '<empty>'}"
        )

    def request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self._request_id += 1
        request_id = self._request_id
        self._read_deadline = time.monotonic() + self._request_timeout()
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        for message in self.events():
            # A response has id and no method. A server-to-client request
            # (session/request_permission and friends) also carries an id, so
            # keying on the id alone either swallows it into notifications,
            # leaving the harness blocked awaiting a permission until the
            # deadline reads as "read exceeded", or, on an id collision,
            # returns the server's request as the response to ours.
            if "method" in message and message.get("id") is not None:
                self.notifications.append(message)
                raise RuntimeError(
                    f"{self.tool} ACP sent server-to-client request "
                    f"{message.get('method')!r} (id {message.get('id')}) during "
                    f"{method!r}; fno answers no server requests yet; "
                    f"stderr: {self.stderr_text or '<empty>'}"
                )
            if message.get("id") == request_id:
                return message
            self.notifications.append(message)
        raise RuntimeError(
            f"{self.tool} ACP stream ended before response id {request_id} for {method!r}; "
            f"stderr: {self.stderr_text or '<empty>'}"
        )

    def result(self, response: dict[str, Any], method: str) -> dict[str, Any]:
        error = response.get("error")
        if isinstance(error, dict):
            detail = error_detail(error, " ")
            raise RuntimeError(f"{self.tool} ACP {method} failed: {detail}")
        result = response.get("result")
        if not isinstance(result, dict):
            raise RuntimeError(f"{self.tool} ACP {method} returned no positive result")
        return result

    def initialize(self) -> dict[str, Any]:
        response = self.request("initialize", initialize_params())
        result = self.result(response, "initialize")
        if result.get("protocolVersion") != 1:
            raise RuntimeError(
                f"{self.tool} ACP initialize did not return the positive "
                "protocolVersion 1 marker"
            )
        return result

    def session_list(self) -> dict[str, Any]:
        response = self.request("session/list", self._session_list_params())
        return self.result(response, "session/list")

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


def initialize_params() -> dict[str, Any]:
    """Return the standard ACP initialize request parameters."""
    return {
        "protocolVersion": 1,
        "clientInfo": {"name": "fno", "version": "0.1.0"},
        "clientCapabilities": {},
    }
