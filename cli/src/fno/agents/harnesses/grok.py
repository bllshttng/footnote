"""Grok's ACP-over-stdio driving lane.

The protocol loop lives in :mod:`fno.agents.harnesses._acp`, shared with the
kimi driver; this module carries grok's identity layer - the argv, the auth
vocabulary, and the caller-assigned bookkeeping id (which is NOT the ACP
session id: ``session/new`` mints its own and ignores this value).
"""
from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any, Optional

from fno.agents.dispatch import DispatchAskError
from fno.agents.harnesses._acp import (
    AcpStdioSession,
    error_detail as _acp_error_detail,
    initialize_params,
    iter_jsonl,  # noqa: F401  (re-exported for the contract test)
)

GROK_DEFAULT_MODEL = "grok-4.6"
GROK_DEFAULT_EFFORT = "high"
AUTH_MARKERS = ("not authenticated", "not signed in", "authentication required")

# A read that cannot time out is a hang, not a wait; the shared core enforces
# the bound through _request_timeout, which reads this at call time so a test
# can shorten it.
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


class GrokStdioSession(AcpStdioSession):
    """A live ``grok agent stdio`` process with correlated ACP requests."""

    tool = "grok"

    def __init__(
        self,
        session_id: str,
        cwd: Path | str,
        *,
        model: Optional[str] = None,
        effort: Optional[str] = None,
        argv: Optional[list[str]] = None,
        env: Optional[dict[str, str]] = None,
    ) -> None:
        super().__init__(
            cwd,
            argv=list(
                argv or grok_stdio_argv(session_id, model=model, effort=effort)
            ),
            env=env,
        )
        self.session_id = session_id

    def _request_timeout(self) -> float:
        return GROK_REQUEST_TIMEOUT_S

    def session_new(self) -> str:
        response = self.request("session/new", session_new_params(self.cwd))
        error = response.get("error")
        if isinstance(error, dict):
            detail = _acp_error_detail(error, " ")
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
