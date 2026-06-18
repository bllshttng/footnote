"""
HermesDriver - wraps the `hermes-agent` CLI.

Mirrors scripts/lib/driver-hermes.sh. Returns is_available()=False when
hermes-agent is not on PATH; the walker treats unavailable drivers as a
hard configuration error, not a runtime failure.
"""
from __future__ import annotations

import re
import shutil
import subprocess
import time

from .base import InvokeResult, UnsupportedDriverMode

_RATE_LIMIT_RE = re.compile(r"rate.?limit|429|too many requests", re.IGNORECASE)
_OVERLOADED_RE = re.compile(r"overloaded|529|server.?overloaded", re.IGNORECASE)
_TIMEOUT_RE = re.compile(r"timeout|connection.*error|connection refused", re.IGNORECASE)
_AUTH_RE = re.compile(
    r"oauth|auth(?:entication)?.{0,20}(?:expired|invalid|failed|denied)|401",
    re.IGNORECASE,
)


def _classify_stderr(stderr: str) -> str | None:
    if _RATE_LIMIT_RE.search(stderr):
        return "rate_limit"
    if _OVERLOADED_RE.search(stderr):
        return "overloaded"
    if _AUTH_RE.search(stderr):
        return "auth"
    if _TIMEOUT_RE.search(stderr):
        return "timeout"
    return "other"


class HermesDriver:
    """Invoke the hermes-agent CLI."""

    name = "hermes"

    def is_available(self) -> bool:
        return shutil.which("hermes-agent") is not None

    def invoke(
        self,
        *,
        prompt: str,
        max_turns: int = 15,
        budget_usd: float = 25.0,
        model: str | None = None,
        cwd: str | None = None,
        timeout_seconds: int = 5400,
    ) -> InvokeResult:
        cmd = [
            "hermes-agent",
            "-p", prompt,
            "--max-iterations", str(max_turns),
        ]
        if model:
            cmd.extend(["--model", model])

        t0 = time.monotonic()
        try:
            proc = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=timeout_seconds,
                cwd=cwd,
            )
            elapsed = time.monotonic() - t0
            rc = proc.returncode
            stdout = proc.stdout or ""
            stderr = proc.stderr or ""

            if rc == 0:
                error_class = None
            else:
                error_class = _classify_stderr(stderr)

            return InvokeResult(
                returncode=rc,
                stdout=stdout,
                stderr=stderr,
                elapsed_seconds=elapsed,
                cost_usd=None,
                error_class=error_class,
            )

        except subprocess.TimeoutExpired:
            elapsed = time.monotonic() - t0
            return InvokeResult(
                returncode=1,
                stdout="",
                stderr=f"timeout after {timeout_seconds}s",
                elapsed_seconds=elapsed,
                cost_usd=None,
                error_class="timeout",
            )

    def invoke_review(self, *, prompt: str, **kwargs) -> InvokeResult:
        """Review mode is not supported by the hermes driver.

        Configure claude-code as the review-mode driver to run sigma-review
        panels inside megawalk's headless invocation.
        """
        raise UnsupportedDriverMode(
            "hermes driver does not support review mode; "
            "configure claude-code as the review-mode driver"
        )
