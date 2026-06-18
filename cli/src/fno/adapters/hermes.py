"""Hermes Agent CLI adapter - concrete RuntimeAdapter implementation.

Plan A3 of the cross-CLI bundle (ab-39195ebd). Second non-Claude adapter;
verifies the pattern established by Plan A2 (Codex, ab-c2194947) generalizes.

Hermes Agent (https://github.com/NousResearch/hermes-agent) is an
open-source AI agent platform with persistent memory and tool-calling.
Unlike Claude Code and Codex which are one-shot CLIs, Hermes carries
memory across invocations by default. The adapter dispatches via
``hermes chat -q "<prompt>"`` and lets Hermes' own memory semantics
apply.

For purely stateless dispatch, configure Hermes server-side to not
persist memory for fno-shaped sessions. The adapter does not
enforce statelessness - that is a Hermes-server concern.

The adapter follows the Codex (Plan A2) implementation pattern: 3
primitives + health, defensive subprocess handling, signal-killed
returncode normalization, body-excerpt truncation, and a separate
``map_hermes_error`` helper that walks Plan A's universal text rules
before falling back to a Hermes-specific exit-code table.

``[VERIFY-AT-IMPL]`` markers in this file flag assumptions made
against ctx7 docs that the implementer should validate against the
real ``hermes`` binary before merge.
"""
from __future__ import annotations

import os
import subprocess
import time
import uuid
from datetime import datetime, timezone

from fno.adapters._shared import create_worktree as _create_worktree
from fno.adapters.base import AdapterCallResult, AdapterHealth, SpawnResult
from fno.adapters.providers.error_taxonomy import (
    ErrorClass,
    NormalizedError,
    classify_error,
    normalize,
)


# Body excerpt cap. Symmetric with NormalizedError.body_excerpt convention
# in error_taxonomy.py (256 bytes).
_BODY_EXCERPT_MAX_BYTES = 256

# Stderr blob cap - prevents unbounded memory if Hermes floods stderr.
_STDERR_MAX_BYTES = 64 * 1024

# Spawn-poll window: how long we wait after Popen before asking whether
# the child has already crashed. Matches the Claude / Codex adapters.
_SPAWN_POLL_SECONDS = 0.5

# `hermes doctor` may probe more subsystems than `codex --version`; give
# it a slightly longer leash than Codex's 10s. [VERIFY-AT-IMPL] tune
# based on real-binary timings.
_HERMES_DOCTOR_TIMEOUT_SECONDS = 15

# Hermes subprocess exit codes. Mirrors Codex's convention; [VERIFY-AT-IMPL]
# confirm against the real binary - Hermes may use different codes.
_HERMES_EXIT_USAGE_ERROR = 1
_HERMES_EXIT_RUNTIME_ERROR = 2
_HERMES_EXIT_AUTH_ERROR = 3
_HERMES_EXIT_TIMEOUT = 124
_HERMES_EXIT_SIGKILL = 137
_HERMES_EXIT_SIGTERM = 143

_RETRYABLE_EXIT_CODES = (
    _HERMES_EXIT_SIGKILL,
    _HERMES_EXIT_SIGTERM,
    _HERMES_EXIT_TIMEOUT,
)

# Substrings that hint at server-side trouble when exit 2 fires without
# an explicit body-text rule match. Case-insensitive substring match.
# Mirrors codex._SERVER_SIDE_HINTS.
_SERVER_SIDE_HINTS = (
    "internal error",
    "internal server error",
    "unavailable",
    "5xx",
    "server",
    "upstream",
)

# Candidate Hermes config directories, ordered most-canonical first.
# [VERIFY-AT-IMPL] ctx7 docs reference `hermes setup` but do not name the
# resulting path. XDG-first, then POSIX home, then macOS-style.
_HERMES_CONFIG_DIR_CANDIDATES = (
    "~/.config/hermes",
    "~/.hermes",
    "~/Library/Application Support/hermes",
)


class HermesCliAdapter:
    """RuntimeAdapter for Hermes Agent (``hermes`` binary).

    Behavior depends on whether we are running inside a CLI agent session:

    - **In-session** (``CLAUDECODE_SESSION_ID`` or ``CODEX_SESSION_ID``
      or ``HERMES_SESSION_ID`` set): shell spawn is FORBIDDEN.
      ``spawn_worker`` returns a ``skill_dispatch_required`` sentinel.
    - **External** (no session env): spawn via ``hermes chat -q "<prompt>"``.

    Auth modes (mirrors Claude/Codex pattern):

    - ``oauth_dir``: provider record ``auth: oauth_dir``,
      credentials_source: one of :data:`_HERMES_CONFIG_DIR_CANDIDATES`.
    - ``api_key``: provider record ``auth: api_key``. Hermes wraps an
      underlying LLM provider; the env var(s) used depend on which
      provider Hermes is configured against. [VERIFY-AT-IMPL] confirm
      the canonical env-var name(s) and treat them as opaque - we
      don't inspect them here.

    **Statefulness caveat:** Hermes carries persistent memory by default
    across invocations. Two parallel ``spawn_worker`` calls to the same
    Hermes server may share memory state. fno does not reason
    about this. For purely stateless dispatch, configure Hermes
    server-side accordingly.
    """

    name: str = "hermes"

    def spawn_worker(self, *, prompt: str, **kwargs) -> SpawnResult:
        """Spawn or refuse to spawn a Hermes worker.

        Returns either the in-session ``skill_dispatch_required`` sentinel,
        a ``spawn_failed`` envelope when the binary is missing or the
        process crashed within :data:`_SPAWN_POLL_SECONDS`, or the live
        worker descriptor ``{worker_id, pid, started_at}``.
        """
        # Fail-closed: an explicitly-empty env var ("") still indicates an
        # outer agent runner. Hermes carries persistent memory by default,
        # so spawning a shell from inside an existing session can corrupt
        # cross-session state. Treat "set to anything, including empty"
        # as in-session.
        if (
            os.environ.get("CLAUDECODE_SESSION_ID") is not None
            or os.environ.get("CODEX_SESSION_ID") is not None
            or os.environ.get("HERMES_SESSION_ID") is not None
        ):
            worker_id = str(uuid.uuid4())
            return {
                "action": "skill_dispatch_required",
                "next_step": f"fno runtime register-worker {worker_id}",
                "worker_id": worker_id,
                "reason": "in-session shell spawn is forbidden; use Agent tool dispatch",
            }

        worker_id = str(uuid.uuid4())
        started_at = datetime.now(timezone.utc).isoformat()
        cmd = ["hermes", "chat", "-q", prompt]

        try:
            proc = subprocess.Popen(
                cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                **{k: v for k, v in kwargs.items() if k in ("env", "cwd")},
            )
        except FileNotFoundError:
            return {
                "action": "spawn_failed",
                "worker_id": worker_id,
                "error": "hermes binary not on PATH",
            }
        except OSError as exc:
            return {
                "action": "spawn_failed",
                "worker_id": worker_id,
                "error": f"hermes spawn rejected by OS: {exc}",
            }

        time.sleep(_SPAWN_POLL_SECONDS)
        rc = proc.poll()
        if rc is not None:
            # Any exit within the poll window is suspicious - even rc==0 is a
            # zombie risk. Drain output with timeout-then-kill so callers
            # never block here.
            try:
                stdout_bytes, stderr_bytes = proc.communicate(timeout=2)
            except subprocess.TimeoutExpired:
                proc.kill()
                try:
                    stdout_bytes, stderr_bytes = proc.communicate(timeout=2)
                except subprocess.TimeoutExpired:
                    stdout_bytes, stderr_bytes = b"", b"(communicate timed out after kill)"
            return {
                "action": "spawn_failed",
                "worker_id": worker_id,
                "pid": proc.pid,
                "returncode": rc,
                "early_exit": True,
                "stdout": (stdout_bytes or b"").decode("utf-8", "replace")[:500],
                "stderr": (stderr_bytes or b"").decode("utf-8", "replace")[:500],
            }

        return {
            "action": "spawned",
            "worker_id": worker_id,
            "pid": proc.pid,
            "started_at": started_at,
        }

    def create_worktree(self, *, name: str, base: str = "main") -> dict:
        """Delegate to the shared CLI-agnostic worktree primitive."""
        return _create_worktree(name=name, base=base)

    def call_api(self, *, command: list[str], retries: int = 3) -> AdapterCallResult:
        """Invoke a Hermes subcommand list with retry on transient failures.

        Examples::

            command=["chat", "-q", "do something"]
            command=["doctor"]
            command=["model"]

        Retries on returncode in :data:`_RETRYABLE_EXIT_CODES`
        (SIGKILL / SIGTERM / timeout). Backoff is exponential
        (``2 ** attempt`` seconds, capped at 8s).

        Returns ``{"stdout": str, "stderr": str, "returncode": int}``.
        """
        cmd = ["hermes"] + list(command)
        last_rc: int | None = None
        last_stdout = ""
        last_stderr = ""

        for attempt in range(retries + 1):
            try:
                result = subprocess.run(
                    cmd,
                    capture_output=True,
                    text=True,
                    check=False,
                )
            except FileNotFoundError:
                return {
                    "ok": False,
                    "stdout": "",
                    "stderr": "hermes binary not on PATH",
                    "returncode": 127,
                }
            except OSError as exc:
                # Permission denied, argument list too long, etc. Surface as a
                # structured error instead of letting the exception propagate
                # to the caller. Matches spawn_worker's OSError envelope.
                return {
                    "ok": False,
                    "stdout": "",
                    "stderr": f"hermes execution failed: {exc}",
                    "returncode": -1,
                }

            if result.returncode == 0:
                return {
                    "ok": True,
                    "stdout": result.stdout,
                    "stderr": result.stderr,
                    "returncode": 0,
                }

            last_rc = result.returncode
            last_stdout = result.stdout
            last_stderr = result.stderr

            # Normalize signal-killed returncodes (-N) to shell-style 128+N
            # BEFORE the retry check so SIGKILL (-9 from subprocess.run /
            # 137 from a shell) classify identically as retryable.
            retry_rc = last_rc
            if retry_rc < 0 and abs(retry_rc) < 128:
                retry_rc = 128 + abs(retry_rc)

            if retry_rc not in _RETRYABLE_EXIT_CODES:
                break

            if attempt < retries:
                time.sleep(min(2 ** attempt, 8))

        final_rc = last_rc if last_rc is not None else -1
        return {
            "ok": final_rc == 0,
            "stdout": last_stdout,
            "stderr": last_stderr,
            "returncode": final_rc,
        }

    def health(self) -> AdapterHealth:
        """Return health status without making a real API call.

        Probes:

        - ``hermes`` binary on PATH
        - ``hermes doctor`` exit code 0 (the documented canonical
          health-check; ctx7 docs do not document a ``--version`` flag).
        - One of :data:`_HERMES_CONFIG_DIR_CANDIDATES` exists.

        [VERIFY-AT-IMPL] confirm canonical config path and narrow the
        candidate list before merge.
        """
        details: dict = {}

        try:
            doctor_result = subprocess.run(
                ["hermes", "doctor"],
                capture_output=True,
                text=True,
                timeout=_HERMES_DOCTOR_TIMEOUT_SECONDS,
            )
        except FileNotFoundError:
            return AdapterHealth(
                ok=False,
                details={
                    "reason": "hermes binary not on PATH",
                    "doctor_exit": None,
                    "doctor_stdout": None,
                    "doctor_stderr": None,
                },
            )
        except OSError as exc:
            # Binary present but cannot be executed (permission denied,
            # etc.). Keep the report stable and informative rather than
            # letting the exception propagate.
            return AdapterHealth(
                ok=False,
                details={
                    "reason": f"hermes doctor execution failed: {exc}",
                    "doctor_exit": None,
                    "doctor_stdout": None,
                    "doctor_stderr": None,
                },
            )
        except subprocess.TimeoutExpired:
            return AdapterHealth(
                ok=False,
                details={
                    "reason": (
                        f"hermes doctor timed out after "
                        f"{_HERMES_DOCTOR_TIMEOUT_SECONDS}s"
                    ),
                    "doctor_exit": None,
                    "doctor_stdout": None,
                    "doctor_stderr": None,
                },
            )

        details["doctor_exit"] = doctor_result.returncode
        details["doctor_stdout"] = (doctor_result.stdout or "").strip()[:500]
        details["doctor_stderr"] = (doctor_result.stderr or "").strip()[:500]

        if doctor_result.returncode != 0:
            return AdapterHealth(
                ok=False,
                details={
                    **details,
                    "reason": (
                        f"hermes doctor exited {doctor_result.returncode}"
                    ),
                },
            )

        config_dir: str | None = None
        stale_paths: list[str] = []
        for candidate in _HERMES_CONFIG_DIR_CANDIDATES:
            expanded = os.path.expanduser(candidate)
            if os.path.isdir(expanded):
                config_dir = expanded
                break
            # Distinguish "doesn't exist" from "exists but is a file or
            # broken symlink" so the operator gets a hint that the path
            # is occupied by the wrong shape rather than missing entirely.
            if os.path.lexists(expanded):
                stale_paths.append(expanded)

        if config_dir is None:
            candidate_list = ", ".join(_HERMES_CONFIG_DIR_CANDIDATES)
            if stale_paths:
                error_msg = (
                    f"hermes config path exists but is not a directory: "
                    f"{', '.join(stale_paths)}; remove it then run `hermes setup`"
                )
            else:
                error_msg = (
                    f"hermes config dir not found (looked in {candidate_list}); "
                    f"run `hermes setup`"
                )
            return AdapterHealth(
                ok=False,
                details={**details, "reason": error_msg},
            )

        details["config_dir"] = config_dir
        return AdapterHealth(ok=True, details=details)


def map_hermes_error(returncode: int, stderr: str) -> NormalizedError:
    """Map a Hermes subprocess outcome to a :class:`NormalizedError`.

    Order of operations (per design doc Locked Decision 4):

    1. **Universal text rules first**. Walk Plan A's :func:`classify_error`
       which knows ``rate limit``, ``too many requests``, ``quota exceeded``,
       ``capacity``, ``overloaded`` (backoff -> PROVIDER_4XX_QUOTA + swap)
       plus ``no credentials`` (cooldown -> PROVIDER_4XX_AUTH + swap).
    2. **Hermes-specific exit-code fallback** for the cases Plan A's
       universal rules don't cover.
    3. **Negative returncode normalization**. Python's :mod:`subprocess`
       returns ``-signal_number`` for signal-killed children on POSIX
       (so SIGKILL surfaces as ``-9`` from :func:`subprocess.run`, while
       the same outcome under a shell appears as ``137``). Negative
       values are normalised to their shell-style counterparts so both
       call paths classify identically.
    4. ``returncode == 0`` is defensive: success outcomes shouldn't reach
       the error mapper, but a stray caller gets ``ErrorClass.UNKNOWN``
       rather than an exception.

    The function never raises. The returned ``body_excerpt`` is truncated
    to :data:`_BODY_EXCERPT_MAX_BYTES` (256 bytes).
    """
    if not isinstance(returncode, int):
        # Defensive: surface caller bugs as UNKNOWN rather than raising.
        return NormalizedError(
            error_class=ErrorClass.UNKNOWN,
            raw_status=None,
            raw_exit_code=None,
            body_excerpt=str(stderr or "")[:_BODY_EXCERPT_MAX_BYTES],
            triggers_swap=False,
        )

    # Normalize signal-killed returncodes (-N) to shell-style 128+N so the
    # rest of the function treats both call paths uniformly.
    if returncode < 0:
        signum = abs(returncode)
        if signum < 128:
            returncode = 128 + signum

    body = stderr or ""
    if len(body) > _STDERR_MAX_BYTES:
        body = body[:_STDERR_MAX_BYTES]
    body_excerpt = body[:_BODY_EXCERPT_MAX_BYTES]

    # Step 1: Plan A universal text classification.
    normalized = normalize(http_status=None, exit_code=returncode, body=body)
    if normalized.error_class is not ErrorClass.UNKNOWN:
        return normalized

    rule = classify_error(status=None, body=body)
    if rule is not None and rule.backoff:
        # backoff rules: "rate limit", "too many requests", "quota
        # exceeded", "capacity", "overloaded". Map to provider_4xx_quota
        # + swap because the next provider in the queue might serve.
        return NormalizedError(
            error_class=ErrorClass.PROVIDER_4XX_QUOTA,
            raw_status=None,
            raw_exit_code=returncode,
            body_excerpt=body_excerpt,
            triggers_swap=True,
        )
    if rule is not None and rule.text == "no credentials":
        return NormalizedError(
            error_class=ErrorClass.PROVIDER_4XX_AUTH,
            raw_status=None,
            raw_exit_code=returncode,
            body_excerpt=body_excerpt,
            triggers_swap=True,
        )

    # Step 2: Hermes-specific exit-code fallback.
    if returncode == 0:
        return NormalizedError(
            error_class=ErrorClass.UNKNOWN,
            raw_status=None,
            raw_exit_code=0,
            body_excerpt=body_excerpt,
            triggers_swap=False,
        )

    if returncode == _HERMES_EXIT_USAGE_ERROR:
        return NormalizedError(
            error_class=ErrorClass.PARSER_ERROR,
            raw_status=None,
            raw_exit_code=returncode,
            body_excerpt=body_excerpt,
            triggers_swap=False,
        )

    if returncode == _HERMES_EXIT_RUNTIME_ERROR:
        lowered = body.lower()
        if any(hint in lowered for hint in _SERVER_SIDE_HINTS):
            return NormalizedError(
                error_class=ErrorClass.PROVIDER_5XX,
                raw_status=None,
                raw_exit_code=returncode,
                body_excerpt=body_excerpt,
                triggers_swap=True,
            )
        return NormalizedError(
            error_class=ErrorClass.UNKNOWN,
            raw_status=None,
            raw_exit_code=returncode,
            body_excerpt=body_excerpt,
            triggers_swap=False,
        )

    if returncode == _HERMES_EXIT_AUTH_ERROR:
        return NormalizedError(
            error_class=ErrorClass.PROVIDER_4XX_AUTH,
            raw_status=None,
            raw_exit_code=returncode,
            body_excerpt=body_excerpt,
            triggers_swap=True,
        )

    if returncode in _RETRYABLE_EXIT_CODES:
        return NormalizedError(
            error_class=ErrorClass.PARSER_ERROR,
            raw_status=None,
            raw_exit_code=returncode,
            body_excerpt=body_excerpt,
            triggers_swap=False,
        )

    return NormalizedError(
        error_class=ErrorClass.UNKNOWN,
        raw_status=None,
        raw_exit_code=returncode,
        body_excerpt=body_excerpt,
        triggers_swap=False,
    )
