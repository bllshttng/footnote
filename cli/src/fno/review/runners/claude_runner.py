"""Claude Code runner for the review orchestrator.

Dispatches a single review agent through ``ClaudeCodeAdapter.spawn_worker``
and parses the structured ``::finding severity file line message::`` stdout
protocol into ``Finding`` instances.

This module provides two entry points:

- ``run_via_claude_code`` - synchronous, testable directly.
- ``make_async_runner``   - wraps ``run_via_claude_code`` in an async
  function suitable for use with ``orchestrate_review_async``.

Finding protocol (one finding per stdout line)::

    ::finding <severity> <file> <line> <message>::

Example::

    ::finding high src/foo.py 42 Missing null check::

Rules enforced here (spec phase-01-async-orchestrator):

- Sentinel ``action == "skill_dispatch_required"``  -> ``ok=False``,
  ``error="in-session-dispatch-not-supported"``
- Spawn failure (``action == "spawn_failed"``)       -> ``ok=False``,
  ``error=<spawn_failed details>``
- Timeout                                            -> ``ok=False``,
  ``error="timeout"``
- Non-empty stdout with zero ``::finding::`` lines   -> single info
  Finding with ``raw=<stdout>`` (NOT silent zero)
- Unparseable finding line                           -> skip that line,
  emit an info Finding with raw=<line>
"""

from __future__ import annotations

import asyncio
import logging
import re
import threading
from typing import Any

from fno.adapters.claude_code import ClaudeCodeAdapter
from fno.review.orchestrator import Finding, WorkerOutcome

log = logging.getLogger(__name__)

# Regex: ::finding <severity> <file> <line> <message>::
# severity: word chars only; file: non-space; line: digits; message: rest up to ::
_FINDING_RE = re.compile(
    r"::finding\s+(\w+)\s+(\S+)\s+(\d+)\s+(.+?)::"
)

DEFAULT_TIMEOUT = 600.0  # seconds


def _parse_findings(agent: str, stdout: str) -> list[Finding]:
    """Parse structured finding lines from stdout.

    Returns a list of Finding instances. If stdout is non-empty but
    contains no ``::finding::`` lines, returns a single info-level
    Finding with ``raw`` populated (no silent zero rule).
    """
    if not stdout.strip():
        return []

    findings: list[Finding] = []
    for line in stdout.splitlines():
        m = _FINDING_RE.search(line)
        if m:
            severity, file_path, line_no, message = m.groups()
            findings.append(Finding(
                agent=agent,
                severity=severity,
                message=message.strip(),
                file=file_path,
                line=int(line_no),
            ))

    if not findings:
        # Non-empty stdout with zero structured findings - capture as raw.
        findings.append(Finding(
            agent=agent,
            severity="info",
            message="unstructured worker output (no ::finding:: lines parsed)",
            raw=stdout,
        ))

    return findings


def run_via_claude_code(
    agent: str,
    prompt: str,
    diff_context: str,
    *,
    timeout: float = DEFAULT_TIMEOUT,
    adapter: ClaudeCodeAdapter | None = None,
    worker_pids: list[int] | None = None,
    json_findings: bool = False,
) -> WorkerOutcome:
    """Invoke a single review agent via ClaudeCodeAdapter.spawn_worker.

    Args:
        agent: Agent name (used to label findings and the outcome).
        prompt: Full composed prompt text for the agent.
        diff_context: Diff text appended to the prompt context.
        timeout: Wall-clock seconds before giving up. Defaults to 600.
        adapter: Optional adapter instance; a fresh one is created when
            not supplied (useful for tests that patch the class).
        worker_pids: Optional list to append the spawned worker's PID to.
            When provided and the spawn result contains a ``pid`` key,
            the PID is appended so the SIGINT handler can reap it.
        json_findings: GATE (cross-model review panel, ab-6c8f4c61). When
            False (default, all-claude OFF path) the legacy ``::finding::``
            parser runs byte-for-byte unchanged and ``provider`` stays unset.
            When True the strict-JSON contract parser runs and the outcome is
            attributed ``provider="claude"``; a non-JSON reply becomes a
            terminal soft per-agent failure. The caller (worker/review.py)
            sets this True only when cross-model is engaged AND appends the
            JSON contract to the prompt.

    Returns:
        A ``WorkerOutcome`` in every code path. Never raises.
    """
    if adapter is None:
        adapter = ClaudeCodeAdapter()

    import time as _time
    started = _time.monotonic()

    # We need a timeout mechanism for the blocking spawn_worker call.
    # Use a thread + event to implement the timeout without requiring
    # the caller to be in an async context.
    result_holder: dict[str, Any] = {}
    exc_holder: dict[str, BaseException] = {}

    def _call() -> None:
        try:
            result_holder["value"] = adapter.spawn_worker(
                prompt=f"{prompt}\n\n---\nDIFF CONTEXT:\n{diff_context}"
            )
        except BaseException as e:
            exc_holder["value"] = e

    thread = threading.Thread(target=_call, daemon=True)
    thread.start()
    thread.join(timeout=timeout)

    elapsed = _time.monotonic() - started

    if thread.is_alive():
        # Timed out - thread is stuck in spawn_worker (e.g. blocking I/O).
        # We can't SIGTERM the stuck child here: spawn_worker hasn't yet
        # returned, so this worker's PID is not in worker_pids yet - every
        # entry in that list belongs to a healthy sibling. Killing them
        # would defeat parallel execution. The outer SIGINT handler
        # (orchestrator._reap_workers) reaps on Ctrl-C; at normal exit,
        # daemon threads + their subprocesses are cleaned up by the
        # kernel when the parent exits.
        log.warning(
            "run_via_claude_code: timeout after %.1fs for agent %s (worker subprocess "
            "may continue until parent exits or ClaudeCodeAdapter exposes mid-call pid)",
            timeout,
            agent,
        )
        return WorkerOutcome(
            agent=agent,
            ok=False,
            error="timeout",
            duration_seconds=elapsed,
        )

    if "value" in exc_holder:
        exc = exc_holder["value"]
        log.error("run_via_claude_code: exception for agent %s: %r", agent, exc)
        return WorkerOutcome(
            agent=agent,
            ok=False,
            error=f"{type(exc).__name__}: {exc}",
            duration_seconds=elapsed,
        )

    spawn_result: dict = result_holder.get("value", {})

    # Track spawned PID for SIGINT reaping (C2 fix).
    if worker_pids is not None and "pid" in spawn_result:
        worker_pids.append(spawn_result["pid"])

    # Handle in-session sentinel
    if spawn_result.get("action") == "skill_dispatch_required":
        log.warning(
            "run_via_claude_code: in-session sentinel received for agent %s "
            "(CLAUDECODE_SESSION_ID is set); cannot shell-spawn from inside a session",
            agent,
        )
        return WorkerOutcome(
            agent=agent,
            ok=False,
            error="in-session-dispatch-not-supported",
            duration_seconds=elapsed,
        )

    # Handle spawn failure
    if spawn_result.get("action") == "spawn_failed":
        error_detail = spawn_result.get("error") or spawn_result.get("stderr") or "spawn_failed"
        log.error("run_via_claude_code: spawn_failed for agent %s: %s", agent, error_detail)
        return WorkerOutcome(
            agent=agent,
            ok=False,
            error=f"spawn_failed: {error_detail}",
            duration_seconds=elapsed,
        )

    # Parse stdout from the result dict (external spawn returns stdout after completion)
    stdout: str = spawn_result.get("stdout", "")

    if json_findings:
        # GATED cross-model path: parse the strict JSON contract and attribute
        # the run to claude. A non-JSON reply is a terminal soft per-agent
        # failure (AC3-ERR). The OFF path below is untouched.
        from fno.review.findings_parser import (
            PARSE_FAILURE_PREFIX,
            FindingsParseError,
            parse_findings_json,
        )

        try:
            findings = parse_findings_json(agent, stdout)
        except FindingsParseError as exc:
            return WorkerOutcome(
                agent=agent,
                ok=False,
                error=f"{PARSE_FAILURE_PREFIX}: {exc} (head={exc.raw_head!r})",
                duration_seconds=elapsed,
                provider="claude",
            )
        return WorkerOutcome(
            agent=agent,
            ok=True,
            findings=findings,
            duration_seconds=elapsed,
            provider="claude",
        )

    # Legacy all-claude path (cross-model OFF): byte-for-byte unchanged.
    findings = _parse_findings(agent, stdout)

    return WorkerOutcome(
        agent=agent,
        ok=True,
        findings=findings,
        duration_seconds=elapsed,
    )


def make_async_runner(
    *,
    timeout: float = DEFAULT_TIMEOUT,
    adapter: ClaudeCodeAdapter | None = None,
    worker_pids: list[int] | None = None,
    json_findings: bool = False,
):
    """Return an async runner function suitable for ``orchestrate_review_async``.

    The returned coroutine wraps ``run_via_claude_code`` via
    ``asyncio.to_thread`` so the blocking spawn call does not block the
    event loop.

    Args:
        timeout: Per-worker timeout in seconds.
        adapter: Optional ClaudeCodeAdapter instance.
        worker_pids: Optional list to receive spawned worker PIDs for
            SIGINT reaping. Threaded through to ``run_via_claude_code``.
        json_findings: GATE (ab-6c8f4c61). False (default) keeps the legacy
            ``::finding::`` path; True routes through the strict-JSON parser.
            See :func:`run_via_claude_code`.
    """
    async def _runner(agent: str, prompt: str, diff_context: str) -> WorkerOutcome:
        return await asyncio.to_thread(
            run_via_claude_code,
            agent,
            prompt,
            diff_context,
            timeout=timeout,
            adapter=adapter,
            worker_pids=worker_pids,
            json_findings=json_findings,
        )

    return _runner
