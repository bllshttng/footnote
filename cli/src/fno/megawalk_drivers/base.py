"""
Base protocol and result type for megawalk drivers.

All concrete drivers must implement the Driver protocol so the walker
(phase 04) and per-node host (phase 03) can dispatch LLM work uniformly
regardless of which agent CLI is installed.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Protocol, runtime_checkable


# ---------------------------------------------------------------------------
# Exceptions
# ---------------------------------------------------------------------------


class DriverError(Exception):
    """Base class for all megawalk driver errors."""


class UnsupportedDriverMode(DriverError):
    """Raised when a driver does not support a requested invocation mode.

    For example, hermes and openclaw raise this from invoke_review() because
    they do not support the multi-turn subagent session needed by sigma-review.
    """


class NoCapableDriver(DriverError):
    """Raised by DriverWithFallback when no driver in the chain supports the mode.

    The walker/host should catch this and park the node with a clear reason
    (e.g. "review_unsupported") rather than propagating the exception.
    """


@dataclass(frozen=True)
class InvokeResult:
    """Immutable result returned by Driver.invoke()."""

    returncode: int
    stdout: str
    stderr: str
    elapsed_seconds: float
    # None if this driver does not track cost (hermes, openclaw stubs)
    cost_usd: float | None
    # One of: 'rate_limit', 'overloaded', 'timeout', 'auth', 'other', None
    # None means success (returncode == 0 and no transient error detected).
    error_class: str | None


@runtime_checkable
class Driver(Protocol):
    """Protocol all megawalk drivers must satisfy."""

    name: str  # 'claude-code' | 'hermes' | 'openclaw'

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
        """Invoke the agent CLI and return a structured result."""
        ...

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
        """Invoke for review-mode work. Multi-turn + subagent enabled.

        Distinct from invoke() so drivers without subagent capability can
        raise UnsupportedDriverMode rather than silently degrade.

        Default max_turns=50 and budget_usd=50.0 are roughly 3x the
        non-review defaults, sized for sigma-review's panel deliberation.
        """
        ...

    def is_available(self) -> bool:
        """Return True if the underlying CLI binary is on PATH."""
        ...
