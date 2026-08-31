"""Kimi's ACP-over-stdio driving lane.

The subprocess owns a held stdin pipe for the life of the session. ACP uses
JSON-RPC response ids for correlation, and notifications can arrive between a
request and its response, so a clean process exit is never a turn or request
receipt.

Kimi mints its own session ids. Version 0.38.0 has no caller-assigned id
anywhere: ``-S/--session [id]`` is resume-only, and no flag names an id for a
new session, so ``session_new`` returns the id the harness minted and the
caller RECORDS it. Do not build an identity guarantee on anything fno passes
in; there is nothing to pass.
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

AUTH_MARKERS = (
    "authentication required",
    "no provider configured",
    "not authenticated",
    "not signed in",
)

# A read that cannot time out is a hang, not a wait. ACP notifications arrive
# between a request and its response, so "no bytes yet" is normal and only a
# DEADLINE distinguishes a working turn from a dead one.
KIMI_REQUEST_TIMEOUT_S = 180.0

# kimi announces the real failure condition on stderr alone. Measured against
# 0.38.0: the JSON-RPC error says only "Authentication required", while stderr
# names the actual state ("no provider configured; complete onboarding via
# /login or the providers endpoint"). The drain thread is async, so a refusal
# composed the instant the error response lands can race the line it is
# supposed to carry; the auth path waits this long for stderr to speak first.
STDERR_SETTLE_S = 2.0


def is_auth_error(text: str) -> bool:
    """One auth predicate over both streams, so every path agrees.

    The JSON-RPC half ("Authentication required") and the stderr half ("no
    provider configured") name the same unfinished onboarding here - the OAuth
    token alone does not satisfy it - so both halves feed one predicate and no
    caller can treat one phrasing as a different failure.
    """
    lowered = text.lower()
    return any(marker in lowered for marker in AUTH_MARKERS)


def _error_detail(error: dict[str, Any], sep: str) -> str:
    """Join an ACP error's message and data so both reach the caller."""
    return sep.join(
        str(part) for part in (error.get("message", error), error.get("data")) if part
    )


class KimiAuthenticationRequired(DispatchAskError):
    """Kimi needs completed onboarding before it can create a session.

    Measured 2026-08-30: the device-code login SUCCEEDED and wrote
    ~/.kimi-code/credentials/kimi-code.json, and session/new still refused,
    because a login mints a token but configures no provider and no model.
    The refusal names the full operator action, never a retry, and never a
    synthesized credential.
    """

    def __init__(self, detail: str = "") -> None:
        message = (
            "kimi onboarding is incomplete: run `kimi login` (device code, "
            "--region mainland-cn or global), then complete provider setup in "
            "the TUI with /login, or add a provider via `kimi provider "
            "catalog` and set default_model in config.toml. A login alone is "
            "measured insufficient."
        )
        if detail:
            message = f"{message} {detail}"
        super().__init__(message, exit_code=13)


def kimi_acp_argv(*, model: Optional[str] = None) -> list[str]:
    """Build kimi's ACP stdio argv.

    There is no session-id argument to pass: kimi mints at session/new and
    hands the id back on the correlated response. ``--model`` is kimi's global
    alias flag (it defaults to default_model in config.toml); `kimi acp
    --model <alias>` parses against 0.38.0. The plain form is measured; the
    alias's effect on an ACP session is read back in the live journey, not
    assumed here.
    """
    argv = ["kimi", "acp"]
    if model:
        argv += ["--model", model]
    return argv


def initialize_params() -> dict[str, Any]:
    """Return the standard ACP initialize request parameters."""
    return {
        "protocolVersion": 1,
        "clientInfo": {"name": "fno", "version": "0.1.0"},
        "clientCapabilities": {},
    }


def session_list_params() -> dict[str, Any]:
    """Empty params: kimi answers session/list unauthenticated (measured)."""
    return {}


def session_new_params(
    cwd: Path | str, add_dirs: Optional[Sequence[Path | str]] = None
) -> dict[str, Any]:
    """Build session/new params, carrying the writable-dirs grant when given.

    kimi declares ``additionalDirectories`` in its ACP sessionCapabilities,
    so the computed writable-dirs set rides the create call the way opencode's
    rides its per-session patch. The auth gate precedes param validation
    (measured: a grant in the params still answers the plain auth error while
    unauthenticated), so the key's acceptance is asserted by the live journey,
    not here.
    """
    params: dict[str, Any] = {"cwd": str(cwd), "mcpServers": []}
    if add_dirs:
        params["additionalDirectories"] = [str(d) for d in add_dirs]
    return params


class KimiAcpSession:
    """A live ``kimi acp`` process with correlated ACP requests."""

    def __init__(
        self,
        cwd: Path | str,
        *,
        model: Optional[str] = None,
        argv: Optional[Sequence[str]] = None,
        env: Optional[dict[str, str]] = None,
    ) -> None:
        self.cwd = Path(cwd)
        self.argv = list(argv or kimi_acp_argv(model=model))
        self.session_id: Optional[str] = None
        self._env = env
        self.proc: Optional[subprocess.Popen[bytes]] = None
        self._events: Optional[Iterator[dict[str, Any]]] = None
        self.notifications: list[dict[str, Any]] = []
        self._stderr: list[str] = []
        self._stderr_thread: Optional[threading.Thread] = None
        self._request_id = 0
        self._read_deadline: Optional[float] = None

    def __enter__(self) -> "KimiAcpSession":
        self.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def start(self) -> "KimiAcpSession":
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

    def _settled_stderr(self) -> str:
        """Wait a bounded moment for stderr before composing a refusal.

        The condition the refusal exists to carry arrives on stderr at roughly
        the same moment as the JSON-RPC error on stdout, and the drain thread
        is async. The wait stops early once a line lands and is bounded so a
        silent child cannot turn a diagnostic into a second hang.
        """
        deadline = time.monotonic() + STDERR_SETTLE_S
        while not self._stderr and time.monotonic() < deadline:
            time.sleep(0.05)
        return self.stderr_text

    def send(self, message: dict[str, Any]) -> None:
        if self.proc is None or self.proc.stdin is None:
            raise RuntimeError("kimi ACP session is not started")
        try:
            self.proc.stdin.write((json.dumps(message) + "\n").encode())
            self.proc.stdin.flush()
        except BrokenPipeError as exc:
            # A dead child must fail typed: dispatch branches on exit codes,
            # and this is the one write path where a raw OSError would escape
            # that contract as an untyped traceback.
            code = self.proc.poll()
            raise DispatchAskError(
                f"kimi ACP child is gone (broken pipe, code {code}); "
                f"stderr: {self._settled_stderr() or '<empty>'}",
                exit_code=code if code else 1,
            ) from exc

    def events(self) -> Iterator[dict[str, Any]]:
        if self.proc is None or self.proc.stdout is None:
            raise RuntimeError("kimi ACP session is not started")
        if self._events is not None:
            return self._events
        stdout = self.proc.stdout
        if not isinstance(stdout, io.BufferedReader):
            # Popen(PIPE) yields a BufferedReader; a stream without read1
            # cannot be bounded, and an unbounded read is the defect this
            # module bans. Refuse the shape; never fall back to it.
            raise RuntimeError(
                f"kimi ACP stdout is not a BufferedReader, so the read "
                f"cannot be bounded; stderr: {self.stderr_text or '<empty>'}"
            )

        def _chunks() -> Iterator[bytes]:
            while True:
                # Bounded by the per-request deadline `request()` publishes. An
                # unbounded read here would hang initialize/session_new/prompt
                # forever whenever kimi went silent without closing stdout.
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
                            f"kimi ACP stdout cannot be watched, so the read "
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
            f"kimi ACP read exceeded {KIMI_REQUEST_TIMEOUT_S:.0f}s with no "
            f"response; stderr: {self.stderr_text or '<empty>'}"
        )

    def request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self._request_id += 1
        request_id = self._request_id
        self._read_deadline = time.monotonic() + KIMI_REQUEST_TIMEOUT_S
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        for message in self.events():
            # A response has id and no method. A server-to-client request
            # (session/request_permission and friends) also carries an id, so
            # keying on the id alone either swallows it into notifications,
            # leaving kimi blocked awaiting a permission until the deadline
            # reads as "read exceeded", or, on an id collision, returns the
            # server's request as the response to ours.
            if "method" in message and message.get("id") is not None:
                self.notifications.append(message)
                raise RuntimeError(
                    f"kimi ACP sent server-to-client request "
                    f"{message.get('method')!r} (id {message.get('id')}) during "
                    f"{method!r}; fno answers no server requests yet; "
                    f"stderr: {self.stderr_text or '<empty>'}"
                )
            if message.get("id") == request_id:
                return message
            self.notifications.append(message)
        raise RuntimeError(
            f"kimi ACP stream ended before response id {request_id} for {method!r}; "
            f"stderr: {self.stderr_text or '<empty>'}"
        )

    @staticmethod
    def result(response: dict[str, Any], method: str) -> dict[str, Any]:
        error = response.get("error")
        if isinstance(error, dict):
            detail = _error_detail(error, " ")
            raise RuntimeError(f"kimi ACP {method} failed: {detail}")
        result = response.get("result")
        if not isinstance(result, dict):
            raise RuntimeError(f"kimi ACP {method} returned no positive result")
        return result

    def initialize(self) -> dict[str, Any]:
        response = self.request("initialize", initialize_params())
        result = self.result(response, "initialize")
        agent_name = (result.get("agentInfo") or {}).get("name")
        if result.get("protocolVersion") != 1 or agent_name != "Kimi Code CLI":
            raise RuntimeError(
                "kimi ACP initialize did not return the positive protocolVersion 1 "
                f"and agentInfo.name 'Kimi Code CLI' markers (got "
                f"protocolVersion={result.get('protocolVersion')!r}, "
                f"agentInfo.name={agent_name!r})"
            )
        return result

    def session_list(self) -> dict[str, Any]:
        response = self.request("session/list", session_list_params())
        return self.result(response, "session/list")

    def session_new(self, add_dirs: Optional[Sequence[Path | str]] = None) -> str:
        """Create a session and return the id kimi MINTED.

        The returned id is the binding: it is handed back synchronously on the
        correlated response, so it is provable rather than scraped. The caller
        records it; nothing fno chose names a kimi session.
        """
        response = self.request("session/new", session_new_params(self.cwd, add_dirs))
        error = response.get("error")
        if isinstance(error, dict):
            detail = _error_detail(error, " ")
            if is_auth_error(detail):
                # The stderr half is the diagnostic half: the JSON-RPC message
                # alone names no condition. Wait the bounded moment so the
                # line is actually in hand before composing the refusal.
                stderr = self._settled_stderr()
                if stderr:
                    detail = f"{detail}; stderr: {stderr}"
                raise KimiAuthenticationRequired(detail)
        result = self.result(response, "session/new")
        session_id = result.get("sessionId")
        if not isinstance(session_id, str) or not session_id:
            raise RuntimeError("kimi ACP session/new returned no positive sessionId marker")
        self.session_id = session_id
        return session_id

    def session_resume(self, session_id: str) -> dict[str, Any]:
        """Resume a minted session by the id kimi itself handed back."""
        response = self.request(
            "session/resume",
            {"sessionId": session_id, "cwd": str(self.cwd), "mcpServers": []},
        )
        return self.result(response, "session/resume")

    def session_close(self, session_id: str) -> dict[str, Any]:
        # Measured 2026-08-31 against 0.38.0 unauthenticated: closing an
        # unknown id answers an empty success rather than an error, so this
        # call never proves the id existed.
        response = self.request("session/close", {"sessionId": session_id})
        return self.result(response, "session/close")

    def session_delete(self, session_id: str) -> dict[str, Any]:
        # Measured 2026-08-31 against 0.38.0 unauthenticated: delete validates
        # the id BEFORE the auth gate and answers a typed -32602 for an
        # unknown one, so its param key (`sessionId`) is pinned by observation.
        response = self.request("session/delete", {"sessionId": session_id})
        return self.result(response, "session/delete")

    def session_fork(self, session_id: str) -> dict[str, Any]:
        response = self.request(
            "session/fork",
            {"sessionId": session_id, "cwd": str(self.cwd), "mcpServers": []},
        )
        return self.result(response, "session/fork")

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
