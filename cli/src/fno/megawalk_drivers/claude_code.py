"""
ClaudeCodeDriver - wraps the `claude --print` CLI.

Command shape mirrors scripts/lib/driver-claude-code.sh lines 35-40:

  claude --print
    --max-turns N
    --max-budget-usd N
    --dangerously-skip-permissions
    [--model MODEL]
    PROMPT
"""
from __future__ import annotations

import re
import shutil
import subprocess
import time

from .base import InvokeResult

# Error classification patterns (case-insensitive)
_RATE_LIMIT_RE = re.compile(r"rate.?limit|429|too many requests", re.IGNORECASE)
_OVERLOADED_RE = re.compile(r"overloaded|529|server.?overloaded", re.IGNORECASE)
_TIMEOUT_RE = re.compile(r"timeout|connection.*error|connection refused", re.IGNORECASE)
_AUTH_RE = re.compile(
    r"oauth|auth(?:entication)?.{0,20}(?:expired|invalid|failed|denied)|401",
    re.IGNORECASE,
)


def _classify_stderr(stderr: str) -> str | None:
    """Return error_class string or None if no transient error detected."""
    if _RATE_LIMIT_RE.search(stderr):
        return "rate_limit"
    if _OVERLOADED_RE.search(stderr):
        return "overloaded"
    if _AUTH_RE.search(stderr):
        return "auth"
    if _TIMEOUT_RE.search(stderr):
        return "timeout"
    return "other"


class ClaudeCodeDriver:
    """Invoke the `claude` CLI with --print mode."""

    name = "claude-code"

    def is_available(self) -> bool:
        return shutil.which("claude") is not None

    def _run(
        self,
        *,
        prompt: str,
        max_turns: int,
        budget_usd: float,
        model: str | None,
        cwd: str | None,
        timeout_seconds: int,
    ) -> InvokeResult:
        cmd = [
            "claude",
            "--print",
            "--max-turns", str(max_turns),
            "--max-budget-usd", str(budget_usd),
            "--dangerously-skip-permissions",
        ]
        if model:
            cmd.extend(["--model", model])
        cmd.append(prompt)

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

            error_class = None if rc == 0 else _classify_stderr(stderr)

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
        return self._run(
            prompt=prompt,
            max_turns=max_turns,
            budget_usd=budget_usd,
            model=model,
            cwd=cwd,
            timeout_seconds=timeout_seconds,
        )

    def invoke_review(
        self,
        *,
        prompt: str,
        max_turns: int = 50,
        budget_usd: float = 50.0,
        model: str | None = None,
        cwd: str | None = None,
        timeout_seconds: int = 1800,
    ) -> InvokeResult:
        """Invoke claude with elevated turn and budget limits for review work.

        The defaults (max_turns=50, budget_usd=50.0) let the sigma-review
        six-agent panel finish deliberating without truncation under the
        lower limits ``invoke`` uses for blueprint/do phases.
        """
        return self._run(
            prompt=prompt,
            max_turns=max_turns,
            budget_usd=budget_usd,
            model=model,
            cwd=cwd,
            timeout_seconds=timeout_seconds,
        )
