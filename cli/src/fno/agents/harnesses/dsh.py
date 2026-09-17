"""DeepSeek Harness (dsh) ACP-over-stdio driving lane.

The protocol loop lives in :mod:`fno.agents.harnesses._acp`, shared with the
grok and kimi drivers; this module carries dsh's identity layer.

dsh mints its own session ids (``randomUUID()`` at session/new) and has no
ACP auth gate: ``authMethods`` is empty and session/new succeeds with no key.
A missing DeepSeek key shows on the first ``session/prompt`` as a JSON-RPC
error (measured against 0.1.5-rc.1, cli/tests/agents/fixtures/dsh-acp-trials.txt).
"""
from __future__ import annotations

from pathlib import Path
from typing import Optional, Sequence

from fno.agents.dispatch import DispatchAskError
from fno.agents.harnesses._acp import AcpStdioSession

# A read that cannot time out is a hang, not a wait; the shared core enforces
# the bound through _request_timeout, which reads this at call time so a test
# can shorten it.
DSH_REQUEST_TIMEOUT_S = 180.0

AUTH_MARKERS = ("no api key for provider route",)


def is_auth_error(text: str) -> bool:
    lowered = text.lower()
    return any(marker in lowered for marker in AUTH_MARKERS)


class DshCredentialRequired(DispatchAskError):
    """dsh has no DeepSeek key, so a turn cannot run."""

    def __init__(self, detail: str = "") -> None:
        message = (
            "dsh has no DeepSeek credential: export an operator-owned "
            "DEEPSEEK_API_KEY in the launching environment, or store it through "
            "the dsh credentials service. fno never synthesizes the key."
        )
        if detail:
            message = f"{message} {detail}"
        super().__init__(message, exit_code=13)


def dsh_acp_argv() -> list[str]:
    """Build dsh's ACP stdio argv.

    No model flag: the acp profile pins its provider route, and a model switch
    rides session/set_config_option, which no caller needs yet.
    """
    return ["dsh", "--profile", "acp"]


class DshAcpSession(AcpStdioSession):
    """A live ``dsh --profile acp`` process with correlated ACP requests."""

    tool = "dsh"
    agent_name = "deepseek-harness-acp"

    def __init__(
        self,
        cwd: Path | str,
        *,
        argv: Optional[Sequence[str]] = None,
        env: Optional[dict[str, str]] = None,
    ) -> None:
        super().__init__(cwd, argv=list(argv or dsh_acp_argv()), env=env)

    def _request_timeout(self) -> float:
        return DSH_REQUEST_TIMEOUT_S

    def _is_auth_error(self, detail: str) -> bool:
        return is_auth_error(detail)

    def _auth_refusal(self, detail: str) -> DispatchAskError:
        return DshCredentialRequired(detail)
