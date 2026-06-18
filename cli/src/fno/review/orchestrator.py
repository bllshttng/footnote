"""Review orchestrator: 6 agent prompts -> findings aggregate.

Phase 03 ships the orchestrator skeleton with synchronous prompt
loading and a fail-closed runner interface. The full parallel
``spawn_worker`` dispatch plus the Haiku confidence scorer are
follow-up work (see :mod:`fno.review.confidence_scorer`).

The surface here is deliberately stable so Phase 04 can wire
``ab review`` into the loop without waiting for the full async
implementation. ``orchestrate_review`` currently runs workers via a
pluggable ``WorkerRunner`` callable so tests and the eventual
``ClaudeCodeAdapter`` dispatch share one code path.
"""

from __future__ import annotations

import asyncio
import os
import signal
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Coroutine, Iterable

PROMPTS_DIR = Path(__file__).parent / "prompts"

# Canonical order matches the sigma-review panel. Report output keeps
# this ordering so diffs between runs are stable.
AGENT_NAMES: tuple[str, ...] = (
    "code_reviewer",
    "silent_failure_hunter",
    "integration_test_analyzer",
    "ux_flow_tester",
    "multi_device_checker",
    "type_design_analyzer",
)


class PromptMissingError(RuntimeError):
    """A required agent prompt file is missing from the bundle."""


@dataclass(frozen=True)
class Finding:
    """A single reviewer observation."""

    agent: str
    severity: str  # critical | high | medium | low | info
    message: str
    file: str | None = None
    line: int | None = None
    confidence: int | None = None  # 0-100, populated by the scorer
    raw: str = ""  # worker's full text output for the finding


@dataclass
class WorkerOutcome:
    """Structured outcome of a single worker invocation."""

    agent: str
    ok: bool
    findings: list[Finding] = field(default_factory=list)
    duration_seconds: float = 0.0
    error: str | None = None
    # Cross-model attribution (cross-model review panel, ab-6c8f4c61).
    # Populated by the runners; rendered by report_builder. Default None so
    # the all-claude path, cache (re)serialization, and existing tests are
    # unchanged. ``note`` carries degradation / fallback context for the
    # report (e.g. "cross-model unavailable: ran on claude") on a SUCCESSFUL
    # run, distinct from ``error`` which carries soft-failure detail.
    provider: str | None = None
    model: str | None = None
    note: str | None = None


@dataclass
class OrchestratorResult:
    """Aggregate result of one ``orchestrate_review`` call."""

    findings: list[Finding]
    workers_completed: int
    workers_failed: int
    suspicious: bool  # all workers returned zero findings
    duration_seconds: float
    outcomes: list[WorkerOutcome] = field(default_factory=list)

    @property
    def verdict_hint(self) -> str:
        """Rough verdict based on finding severities.

        The report builder owns the final verdict (it may downgrade to
        ``done-with-concerns`` or escalate to ``blocked`` based on the
        confidence-scored subset). This hint is what the orchestrator
        sees from the raw worker output.
        """
        critical = sum(1 for f in self.findings if f.severity == "critical")
        high = sum(1 for f in self.findings if f.severity == "high")
        if critical > 0:
            return "blocked"
        if high > 0:
            return "done-with-concerns"
        if self.suspicious:
            return "done-with-concerns"  # all-clean is suspicious by default
        return "ready-to-merge"


# --- Prompt loading ------------------------------------------------------


def _strip_frontmatter(text: str, path: Path) -> str:
    """Strip a leading YAML frontmatter block from an agent definition.

    Agent prompt files at ``cli/src/fno/review/prompts/*.md`` begin
    with a ``---\\n... ---\\n`` YAML frontmatter that carries metadata
    (name, description, model) used by other tooling but irrelevant to the
    body passed as a system prompt to ``claude -p``.

    Without this strip, the loaded prompt body starts with ``---``; once
    composed with ``f"{body}\\n\\n---\\nDIFF CONTEXT:\\n{diff}"`` and passed
    as an argv element to ``claude``, the parser interprets the leading
    ``---`` as an unknown option and the worker reports
    ``spawn_failed: error: unknown option '---'``. Every worker fails; the
    panel writes a verdict-``done-with-concerns`` artifact indistinguishable
    from a real clean review.

    A fence-presence assertion fails loud (``PromptMissingError``) if the
    file has no opening ``---\\n`` or no closing fence, so a malformed
    agent definition does not silently dispatch garbage.
    """
    head = "---\n"
    if not text.startswith(head):
        raise PromptMissingError(
            f"agent file missing YAML frontmatter (no leading '---\\n'): {path}"
        )
    closing_idx = text.find("\n---\n", len(head))
    if closing_idx < 0:
        raise PromptMissingError(
            f"agent file YAML frontmatter not closed (no second '---\\n'): {path}"
        )
    return text[closing_idx + len("\n---\n"):]


def load_prompts(prompts_dir: Path | None = None) -> dict[str, str]:
    """Read all 6 bundled agent prompts, stripping YAML frontmatter.

    The strip prevents the body from starting with ``---``, which would
    otherwise crash ``claude -p`` with ``unknown option '---'`` when the
    composed prompt is forwarded as an argv element. See
    ``_strip_frontmatter`` for the full rationale.

    Raises:
        PromptMissingError: any named prompt is missing or its YAML
            frontmatter is malformed; the review cannot be run without the
            full panel.
    """
    resolved = prompts_dir or PROMPTS_DIR
    # Two-pass: verify every named file exists before reading any. Preserves
    # the legacy contract that a missing-file diagnostic names the
    # first-missing agent regardless of whether earlier-in-order files
    # have malformed frontmatter.
    paths: dict[str, Path] = {}
    for name in AGENT_NAMES:
        path = resolved / f"{name}.md"
        if not path.exists():
            raise PromptMissingError(f"prompt missing: {path}")
        paths[name] = path
    out: dict[str, str] = {}
    for name, path in paths.items():
        out[name] = _strip_frontmatter(path.read_text(encoding="utf-8"), path)
    return out


# --- Worker runner ABI ---------------------------------------------------

WorkerRunner = Callable[[str, str, str], WorkerOutcome]
"""Callable contract for invoking a single worker.

Signature: ``(agent_name, prompt_text, diff_context) -> WorkerOutcome``.
The orchestrator cares only that it can call this once per agent and
receive a structured outcome. The real implementation will dispatch
``claude -p <composed-prompt>`` via ClaudeCodeAdapter; tests supply a
lambda that returns canned findings.
"""


def _default_worker_runner(agent: str, prompt: str, diff_context: str) -> WorkerOutcome:
    """Placeholder runner used when no runner is supplied.

    Raises ``NotImplementedError`` so an accidental production call
    surfaces immediately rather than silently returning zero findings
    and letting a "clean" review satisfy the quality gate.
    """
    raise NotImplementedError(
        "no WorkerRunner supplied; pass runner= from the caller. "
        "See cli/src/fno/review/orchestrator.py for the ABI."
    )


# --- SIGINT handler + worker reap helper ---------------------------------

def _reap_workers(pids: list[int], *, sigterm_grace: float = 5.0) -> None:
    """Send SIGTERM to every tracked PID, then SIGKILL survivors after grace.

    Design notes:
    - Uses only async-signal-safe builtins (os.kill, os.waitpid, time.monotonic).
    - ProcessLookupError (pid already dead) is swallowed silently.
    - ChildProcessError (not a child of this process) is swallowed; kernel cleans up.
    - No logging, no locks - safe to call from a signal handler.

    Args:
        pids: List of OS pids to reap.
        sigterm_grace: Seconds to wait for polite exit before SIGKILL.
    """
    if not pids:
        return

    # Phase 1: send SIGTERM to all
    for pid in pids:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass  # already dead
        except PermissionError:
            pass  # not our process, skip

    # Phase 2: poll for polite exits up to sigterm_grace seconds
    survivors: list[int] = []
    deadline = time.monotonic() + sigterm_grace
    remaining = list(pids)

    while remaining and time.monotonic() < deadline:
        still_alive: list[int] = []
        for pid in remaining:
            try:
                result_pid, _ = os.waitpid(pid, os.WNOHANG)
                if result_pid == 0:
                    # Not exited yet
                    still_alive.append(pid)
                # result_pid == pid means it exited politely - done
            except ChildProcessError:
                pass  # not our child, skip
            except ProcessLookupError:
                pass  # already dead
        remaining = still_alive
        if remaining:
            time.sleep(0.05)

    survivors = remaining

    # Phase 3: SIGKILL survivors
    for pid in survivors:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass  # died between grace and now
        except PermissionError:
            pass

        try:
            os.waitpid(pid, 0)  # blocking wait to prevent zombie
        except ChildProcessError:
            pass  # not our child
        except ProcessLookupError:
            pass


# --- orchestrate_review --------------------------------------------------

def orchestrate_review(
    diff_context: str,
    *,
    prompts: dict[str, str] | None = None,
    runner: WorkerRunner = _default_worker_runner,
    agents: Iterable[str] | None = None,
) -> OrchestratorResult:
    """Run every agent against ``diff_context`` and aggregate findings.

    The current implementation is synchronous - it invokes ``runner``
    once per agent. The follow-up async orchestrator will parallelize
    while preserving this return shape.
    """
    resolved_prompts = prompts or load_prompts()
    selected = list(agents) if agents else list(AGENT_NAMES)

    findings: list[Finding] = []
    outcomes: list[WorkerOutcome] = []
    completed = 0
    failed = 0
    started = time.monotonic()

    for agent in selected:
        if agent not in resolved_prompts:
            raise PromptMissingError(f"no prompt for agent: {agent}")
        before = time.monotonic()
        try:
            outcome = runner(agent, resolved_prompts[agent], diff_context)
        except Exception as exc:
            outcome = WorkerOutcome(
                agent=agent,
                ok=False,
                duration_seconds=time.monotonic() - before,
                error=f"{type(exc).__name__}: {exc}",
            )
        if outcome.duration_seconds == 0.0:
            outcome.duration_seconds = time.monotonic() - before
        outcomes.append(outcome)
        if outcome.ok:
            completed += 1
            findings.extend(outcome.findings)
        else:
            failed += 1

    elapsed = time.monotonic() - started
    suspicious = completed > 0 and not findings

    return OrchestratorResult(
        findings=findings,
        workers_completed=completed,
        workers_failed=failed,
        suspicious=suspicious,
        duration_seconds=elapsed,
        outcomes=outcomes,
    )


# --- orchestrate_review_async / orchestrate_review_parallel --------------

# Type alias for an async worker runner callable.
AsyncWorkerRunner = Callable[
    [str, str, str],
    Coroutine[Any, Any, WorkerOutcome],
]

_SEMAPHORE_SIZE = 6


async def orchestrate_review_async(
    diff_context: str,
    *,
    prompts: dict[str, str] | None = None,
    runner: AsyncWorkerRunner,
    agents: Iterable[str] | None = None,
) -> OrchestratorResult:
    """Async orchestrator: dispatches all workers in parallel via asyncio.gather.

    Concurrency is bounded by a semaphore of size 6 (matching the panel
    size). Exceptions from individual workers are caught via
    ``return_exceptions=True`` and converted to failed ``WorkerOutcome``
    instances - no worker failure propagates past this function.

    Per-worker ``started_at`` timestamps are recorded so callers can assert
    parallelism (all workers start before any completes).

    Args:
        diff_context: Diff text passed to every worker.
        prompts: Agent prompt map. Loaded from bundled files when omitted.
        runner: Async callable ``(agent, prompt, diff) -> WorkerOutcome``.
        agents: Subset of AGENT_NAMES to run. Defaults to all six.

    Returns:
        ``OrchestratorResult`` aggregating all outcomes in AGENT_NAMES order.
    """
    resolved_prompts = prompts or load_prompts()
    selected = list(agents) if agents else list(AGENT_NAMES)

    semaphore = asyncio.Semaphore(_SEMAPHORE_SIZE)
    started_at: dict[str, float] = {}

    async def _run_one(agent: str) -> WorkerOutcome:
        if agent not in resolved_prompts:
            raise PromptMissingError(f"no prompt for agent: {agent}")
        async with semaphore:
            before = time.monotonic()
            started_at[agent] = before
            outcome = await runner(agent, resolved_prompts[agent], diff_context)
            if outcome.duration_seconds == 0.0:
                outcome.duration_seconds = time.monotonic() - before
            return outcome

    wall_start = time.monotonic()
    raw_results: list[WorkerOutcome | BaseException] = await asyncio.gather(
        *[_run_one(agent) for agent in selected],
        return_exceptions=True,
    )

    # Build outcomes in AGENT_NAMES order (same order as ``selected``).
    outcomes: list[WorkerOutcome] = []
    for agent, raw in zip(selected, raw_results):
        if isinstance(raw, BaseException):
            outcomes.append(WorkerOutcome(
                agent=agent,
                ok=False,
                error=f"{type(raw).__name__}: {raw}",
                duration_seconds=time.monotonic() - started_at.get(agent, wall_start),
            ))
        else:
            outcomes.append(raw)

    findings: list[Finding] = []
    completed = 0
    failed = 0
    for outcome in outcomes:
        if outcome.ok:
            completed += 1
            findings.extend(outcome.findings)
        else:
            failed += 1

    elapsed = time.monotonic() - wall_start
    suspicious = completed > 0 and not findings

    return OrchestratorResult(
        findings=findings,
        workers_completed=completed,
        workers_failed=failed,
        suspicious=suspicious,
        duration_seconds=elapsed,
        outcomes=outcomes,
    )


def orchestrate_review_parallel(
    diff_context: str,
    *,
    prompts: dict[str, str] | None = None,
    runner: AsyncWorkerRunner,
    agents: Iterable[str] | None = None,
    session_id: str | None = None,
    artifacts_dir: Path | None = None,
    cache_enabled: bool = True,
    git_sha_value: str | None = None,
    scratchpad_path: Path | None = None,
    worker_pids: list[int] | None = None,
    provider_set: Iterable[str] | None = None,
) -> OrchestratorResult:
    """Synchronous wrapper around ``orchestrate_review_async``.

    Calls ``asyncio.run`` so this is safe to call from any synchronous
    context. Do not call from inside an already-running event loop
    (use ``await orchestrate_review_async(...)`` directly instead).

    When both ``session_id`` and ``artifacts_dir`` are provided, an
    exclusive ``fcntl.flock`` is held across the full orchestration.
    A second concurrent call on the same session raises
    :exc:`~fno.review.locking.ReviewLockBusy` so the caller
    (``worker/review.py``) can exit 11 with a structured diagnostic.

    When either kwarg is omitted, locking is skipped (back-compat).

    Cache lookup happens inside the lock (when lock is held) so two
    concurrent processes cannot both hit + write the same cache entry.
    Cache is skipped when ``cache_enabled=False``, when ``session_id``
    or ``artifacts_dir`` is None, or when the result has failures/is
    suspicious (bad runs must not be memoized).

    A SIGINT handler is installed for the duration of the orchestration.
    On Ctrl-C: all tracked worker PIDs are reaped (SIGTERM then SIGKILL),
    a message is printed to stderr, and the process exits 130. The
    scratchpad directory (if provided) is NOT deleted - forensics depend
    on it. The prior SIGINT handler is restored on normal exit via
    try/finally.

    Args:
        cache_enabled: Set to ``False`` to bypass both read and write.
        git_sha_value: Override for the git HEAD SHA (for tests that want
            deterministic cache keys without a real git repo).
        scratchpad_path: Optional path to the scratchpad directory. Printed
            in the SIGINT message so the operator knows where to find
            forensics after an interrupted run.
    """
    resolved_prompts = prompts or load_prompts()

    # PID list shared with the SIGINT handler. Callers may supply a pre-
    # populated list (e.g. a test that spawns a real subprocess before
    # calling orchestrate_review_parallel) so the handler reaps those PIDs
    # too. When None, we start with an empty list that runners can append to
    # as they spawn workers.
    _tracked_pids: list[int] = worker_pids if worker_pids is not None else []

    # --- Install SIGINT handler -------------------------------------------
    # Re-entrant safety: we swap the handler to SIG_IGN as the first action
    # inside the handler body so a second SIGINT during reap is ignored.
    #
    # Platform note: asyncio loop.add_signal_handler() is preferred because
    # it interacts correctly with the running event loop; signal.signal() is
    # the fallback for platforms where loop signal support is unavailable
    # (e.g. Windows, or non-main threads). Since orchestrate_review_parallel
    # is always called from the main thread we prefer the asyncio path, but
    # we detect it at runtime to be safe.
    #
    # We use signal.signal() here (not loop.add_signal_handler) because
    # this function is synchronous - the event loop isn't running yet when
    # we install the handler. asyncio.run() starts the loop internally, and
    # signals delivered to the main thread while the loop runs are handled
    # by the signal module handler (not loop.add_signal_handler which is
    # only effective for handlers registered on a running loop from within
    # a coroutine). Using signal.signal() from the main thread before
    # asyncio.run() is the correct pattern.

    _handler_installed = False

    def _sigint_handler(signum: int, frame: object) -> None:
        # Re-entrant guard: replace ourselves with SIG_IGN immediately.
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        n = len(_tracked_pids)
        _reap_workers(_tracked_pids)
        if scratchpad_path is not None:
            print(
                f"review: reaped {n} workers, scratchpad at {scratchpad_path}",
                file=sys.stderr,
            )
        else:
            print(f"review: reaped {n} workers (no scratchpad)", file=sys.stderr)
        sys.exit(130)

    prior_handler = signal.signal(signal.SIGINT, _sigint_handler)

    def _run() -> OrchestratorResult:
        from fno.review import cache as _cache

        # Determine whether caching is possible for this invocation.
        can_cache = (
            cache_enabled
            and session_id is not None
            and artifacts_dir is not None
        )

        resolved_sha: str | None = None
        key: str | None = None

        if can_cache:
            resolved_sha = git_sha_value if git_sha_value is not None else _cache.git_sha()
            ph = _cache.prompt_hash(resolved_prompts)
            key = _cache.cache_key(session_id, resolved_sha, ph, provider_set)

            # Cache read - attempt to return a cached result.
            cached_body = _cache.read_cache(key, artifacts_dir=artifacts_dir)
            if cached_body is not None:
                try:
                    return _cache.reconstruct_result(cached_body)
                except Exception as exc:  # noqa: BLE001
                    print(
                        f"[cache] deserialization failed, re-running: {exc}",
                        file=sys.stderr,
                    )

        # Cache miss (or cache disabled) - run the orchestration.
        result = asyncio.run(
            orchestrate_review_async(
                diff_context,
                prompts=resolved_prompts,
                runner=runner,
                agents=agents,
            )
        )

        # Cache write - only for clean runs. Key the WRITE by the ACTUAL
        # per-agent routing (read off the outcomes), not the requested routing:
        # a run where a pinned provider fell back to claude writes under the
        # actual (claude) routing, so a later read keyed on the requested
        # provider misses it and re-runs once that provider recovers, instead
        # of serving the claude-fallback result as if it were cross-modeled
        # (codex review P2). For a clean run actual == requested, so the write
        # key equals the read key and caching works as before. On the all-claude
        # OFF path no outcome carries a provider, so this falls back to
        # ``provider_set`` (None) and the legacy key is reproduced exactly.
        if can_cache and key is not None and resolved_sha is not None:
            if result.workers_failed == 0 and not result.suspicious:
                try:
                    actual_dim = (
                        sorted(
                            f"{o.agent}={o.provider}"
                            for o in result.outcomes
                            if o.provider
                        )
                        or provider_set
                    )
                    write_key = _cache.cache_key(session_id, resolved_sha, ph, actual_dim)
                    body = _cache.build_cache_body(
                        write_key, session_id, resolved_sha, result, provider_set=actual_dim
                    )
                    _cache.write_cache(write_key, body, artifacts_dir=artifacts_dir)
                except Exception as exc:  # noqa: BLE001
                    print(
                        f"[cache] write failed for key {key[:16]}...: {exc}",
                        file=sys.stderr,
                    )

        return result

    try:
        if session_id is not None and artifacts_dir is not None:
            from fno.review.locking import acquire_review_lock
            with acquire_review_lock(session_id, artifacts_dir=artifacts_dir):
                return _run()
        return _run()
    finally:
        # Restore the prior SIGINT handler unconditionally (normal exit path).
        # If the handler fired and called sys.exit(130) this finally block
        # still runs before the SystemExit propagates, but the prior_handler
        # restore is harmless - the process is exiting anyway.
        signal.signal(signal.SIGINT, prior_handler)
