"""Kimi's ACP-over-stdio driving lane.

The protocol loop lives in :mod:`fno.agents.harnesses._acp`, shared with the
grok driver; this module carries kimi's identity layer - the argv, the auth
vocabulary, the positive markers, and the stderr diagnostic seam.

Kimi mints its own session ids. Version 0.38.0 has no caller-assigned id
anywhere: ``-S/--session [id]`` is resume-only, and no flag names an id for a
new session, so ``session_new`` returns the id the harness minted and the
caller RECORDS it. Do not build an identity guarantee on anything fno passes
in; there is nothing to pass.
"""
from __future__ import annotations

import time
from pathlib import Path
from typing import Any, Optional, Sequence

from fno.agents.dispatch import DispatchAskError
from fno.agents.harnesses._acp import (
    AcpStdioSession,
    initialize_params,  # noqa: F401  (re-exported for the contract test)
    iter_jsonl,  # noqa: F401  (re-exported for the contract test)
)

# A read that cannot time out is a hang, not a wait; the shared core enforces
# the bound through _request_timeout, which reads this at call time so a test
# can shorten it.
KIMI_REQUEST_TIMEOUT_S = 180.0

AUTH_MARKERS = (
    "authentication required",
    "no provider configured",
    "not authenticated",
    "not signed in",
)

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


class KimiAcpSession(AcpStdioSession):
    """A live ``kimi acp`` process with correlated ACP requests."""

    tool = "kimi"
    agent_name = "Kimi Code CLI"

    def __init__(
        self,
        cwd: Path | str,
        *,
        model: Optional[str] = None,
        argv: Optional[Sequence[str]] = None,
        env: Optional[dict[str, str]] = None,
    ) -> None:
        super().__init__(cwd, argv=list(argv or kimi_acp_argv(model=model)), env=env)

    def _request_timeout(self) -> float:
        return KIMI_REQUEST_TIMEOUT_S

    def _session_list_params(self) -> dict[str, Any]:
        return session_list_params()

    def _settled_stderr(self) -> str:
        """Wait a bounded moment for stderr before composing a refusal.

        The condition the refusal exists to carry arrives on stderr at roughly
        the same moment as the JSON-RPC error on stdout, and the drain thread
        is async. The wait stops early once a line lands, and once the child
        is dead (its stderr is final), and is bounded so a silent child cannot
        turn a diagnostic into a second hang.
        """
        deadline = time.monotonic() + STDERR_SETTLE_S
        while not self._stderr and time.monotonic() < deadline:
            time.sleep(0.05)
            if self.proc is not None and self.proc.poll() is not None:
                # A dead child's stderr is final once the drain thread has
                # finished; no line will ever arrive, so do not spend the
                # whole settle waiting for one.
                if self._stderr_thread is not None:
                    self._stderr_thread.join(timeout=0.5)
                break
        return self.stderr_text

    def _is_auth_error(self, detail: str) -> bool:
        return is_auth_error(detail)

    def _auth_refusal(self, detail: str) -> DispatchAskError:
        # The stderr half is the diagnostic half: the JSON-RPC message alone
        # names no condition. Wait the bounded moment so the line is actually
        # in hand before composing the refusal.
        stderr = self._settled_stderr()
        if stderr:
            detail = f"{detail}; stderr: {stderr}"
        return KimiAuthenticationRequired(detail)

    def session_new(self, add_dirs: Optional[Sequence[Path | str]] = None) -> str:  # type: ignore[override]
        return super().session_new(session_new_params(self.cwd, add_dirs))

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
