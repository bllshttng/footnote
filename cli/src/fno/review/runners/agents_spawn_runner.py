"""codex/gemini review runner via one-shot ``fno agents spawn`` (ab-6c8f4c61).

Mirrors :mod:`fno.review.runners.claude_runner`'s shape - a synchronous
``run_via_agents_spawn`` that returns a :class:`WorkerOutcome` in EVERY path
(never raises), plus a ``make_async_runner`` that wraps it via
``asyncio.to_thread``. The difference is the dispatch mechanism: codex/gemini
agents run through ``dispatch_spawn(once=True)`` (an ephemeral one-shot that
returns the model's reply text directly), and findings come from the shared
strict-JSON parser rather than the ``::finding::`` stdout protocol.

Failure handling (Failure Modes / Errors + Concurrency):

- A non-JSON / malformed reply -> :class:`FindingsParseError` -> a *terminal*
  soft per-agent failure (``ok=False``, error prefixed ``findings-parse-failed``).
  The selector does NOT retry this on claude (AC3-ERR: the provider answered,
  just not in contract); it is recorded and shown in the report.
- A provider spawn failure / lockout (``DispatchAskError``) -> a *retryable*
  soft failure (error prefixed ``dispatch-failed``). The selector falls through
  the provider chain / degrades to claude (AC5-FR).
- A per-agent timeout (one hung provider) -> a retryable soft failure
  (``timeout``), so the panel never blocks on one slow provider.
"""
from __future__ import annotations

import asyncio
import logging
import time as _time
import uuid
from pathlib import Path
from typing import Any, Callable, Optional

from fno.review.findings_parser import (
    PARSE_FAILURE_PREFIX,
    FindingsParseError,
    parse_findings_json,
)
from fno.review.orchestrator import WorkerOutcome

log = logging.getLogger(__name__)

DEFAULT_TIMEOUT = 600.0  # seconds, mirrors claude_runner
_FROM_NAME = "fno-review"

# Error-string prefixes that let the selector distinguish a terminal parse
# failure (do not retry) from a retryable dispatch/timeout failure (fall
# through to claude). PARSE_FAILURE_PREFIX is the shared parser-module constant
# (re-exported here for callers/tests that import it from this runner). See
# :func:`is_retryable_failure`.
DISPATCH_FAILURE_PREFIX = "dispatch-failed"
TIMEOUT_FAILURE = "timeout"


def is_retryable_failure(outcome: WorkerOutcome) -> bool:
    """True when a spawn-runner soft-fail should fall through to claude.

    Dispatch/lockout/timeout failures are retryable on claude (AC5-FR); a parse
    failure is terminal (AC3-ERR: the provider replied, just not in the JSON
    contract, so re-running it on claude would mask a real reviewer that ran).
    """
    if outcome.ok or not outcome.error:
        return False
    return not outcome.error.startswith(PARSE_FAILURE_PREFIX)


def _spawn_name(agent: str, provider: str) -> str:
    """Unique, validation-safe agent name for a one-shot review dispatch.

    A uuid suffix avoids registry collisions across concurrent panels (the
    one-shot tears its row down after, but two panels could overlap). The whole
    name is never an 8-hex short-id, and contains no path / env-unsafe chars.
    """
    return f"review-{agent}-{provider}-{uuid.uuid4().hex[:8]}"


def run_via_agents_spawn(
    agent: str,
    prompt: str,
    diff_context: str,
    *,
    provider: str,
    cwd: Path,
    timeout: float = DEFAULT_TIMEOUT,
    dispatch: Optional[Callable[..., Any]] = None,
) -> WorkerOutcome:
    """Dispatch one review agent on ``provider`` (codex/gemini) via spawn --once.

    Args:
        agent: agent name (labels findings + outcome).
        prompt: composed agent prompt text.
        diff_context: diff appended to the prompt (same shape as claude_runner).
        provider: ``"codex"`` or ``"gemini"``.
        cwd: working directory for the spawned agent.
        timeout: per-agent wall-clock ceiling. Passed to ``dispatch_spawn`` AND
            used as the hard thread-join ceiling so one hung provider cannot
            stall the panel.
        dispatch: injectable ``dispatch_spawn`` callable (tests pass a fake).

    Returns:
        A :class:`WorkerOutcome` in every path. Never raises.
    """
    if dispatch is None:
        from fno.agents.dispatch import dispatch_spawn as dispatch

    started = _time.monotonic()
    composed = f"{prompt}\n\n---\nDIFF CONTEXT:\n{diff_context}"
    name = _spawn_name(agent, provider)

    # Hard timeout ceiling via a daemon thread, mirroring claude_runner: a hung
    # provider returns a soft "timeout" rather than blocking the gather.
    import threading

    result_holder: dict[str, Any] = {}
    exc_holder: dict[str, BaseException] = {}

    def _call() -> None:
        try:
            result_holder["value"] = dispatch(
                name=name,
                message=composed,
                provider=provider,
                cwd=cwd,
                once=True,
                # Floor at 1s: int(0.x) == 0, and a 0 timeout is "no timeout"
                # in many subprocess APIs, which would let a hung provider's
                # subprocess outlive the thread-join ceiling (gemini review).
                timeout=max(1, int(timeout)),
                from_name=_FROM_NAME,
            )
        except BaseException as e:  # noqa: BLE001 - funnel into WorkerOutcome
            exc_holder["value"] = e

    thread = threading.Thread(target=_call, daemon=True)
    thread.start()
    thread.join(timeout=timeout)
    elapsed = _time.monotonic() - started

    if thread.is_alive():
        log.warning(
            "agents_spawn_runner: timeout after %.1fs for agent %s on %s",
            timeout, agent, provider,
        )
        return WorkerOutcome(
            agent=agent, ok=False, error=TIMEOUT_FAILURE,
            duration_seconds=elapsed, provider=provider,
        )

    if "value" in exc_holder:
        exc = exc_holder["value"]
        # DispatchAskError (lockout / spawn fail / daemon required) -> retryable.
        exit_code = getattr(exc, "exit_code", None)
        suffix = f"[exit={exit_code}]" if exit_code is not None else ""
        log.warning(
            "agents_spawn_runner: dispatch failed for agent %s on %s: %r",
            agent, provider, exc,
        )
        return WorkerOutcome(
            agent=agent,
            ok=False,
            error=f"{DISPATCH_FAILURE_PREFIX}{suffix}: {exc}",
            duration_seconds=elapsed,
            provider=provider,
        )

    spawn_result = result_holder.get("value")
    reply = getattr(spawn_result, "reply", None)
    if not isinstance(reply, str):
        # Contract breach: a codex/gemini once-dispatch must return reply text.
        return WorkerOutcome(
            agent=agent,
            ok=False,
            error=f"{DISPATCH_FAILURE_PREFIX}: provider returned no reply text",
            duration_seconds=elapsed,
            provider=provider,
        )

    try:
        findings = parse_findings_json(agent, reply)
    except FindingsParseError as exc:
        # Terminal soft-fail: the provider answered, just not in the JSON
        # contract. Recorded + reported, never retried (AC3-ERR).
        return WorkerOutcome(
            agent=agent,
            ok=False,
            error=f"{PARSE_FAILURE_PREFIX}: {exc} (head={exc.raw_head!r})",
            duration_seconds=elapsed,
            provider=provider,
        )

    return WorkerOutcome(
        agent=agent,
        ok=True,
        findings=findings,
        duration_seconds=elapsed,
        provider=provider,
    )


def make_async_runner(
    *,
    provider: str,
    cwd: Path,
    timeout: float = DEFAULT_TIMEOUT,
    dispatch: Optional[Callable[..., Any]] = None,
):
    """Return an async runner ``(agent, prompt, diff) -> WorkerOutcome``.

    Wraps the blocking ``run_via_agents_spawn`` via ``asyncio.to_thread`` so the
    spawn call does not block the event loop (mirrors claude_runner).
    """

    async def _runner(agent: str, prompt: str, diff_context: str) -> WorkerOutcome:
        return await asyncio.to_thread(
            run_via_agents_spawn,
            agent,
            prompt,
            diff_context,
            provider=provider,
            cwd=cwd,
            timeout=timeout,
            dispatch=dispatch,
        )

    return _runner
