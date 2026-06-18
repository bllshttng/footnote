"""Codex CLI adapter - concrete RuntimeAdapter implementation.

First non-Claude CLI adapter (Plan A2 of the cross-CLI bundle, ab-c2194947).
Mirrors ``claude_code.py`` structurally because the abstraction
(3 primitives + health) is intentionally narrow. The only meaningful
divergence is which binary gets invoked and how its exit codes map to
fno' closed ``ErrorClass`` enum.
"""
from __future__ import annotations

import os
import re
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


# Minimum supported Codex CLI version (verified during /think at 0.117.0).
# health() returns ok=False if older.
MIN_CODEX_VERSION = "0.117.0"

# Body excerpt cap. Symmetric with NormalizedError.body_excerpt convention
# in error_taxonomy.py (256 bytes) and with the design doc's
# "must clamp model identifiers to 256 bytes at the producer" rule.
_BODY_EXCERPT_MAX_BYTES = 256

# Stderr blob cap - prevents unbounded memory if Codex floods stderr.
_STDERR_MAX_BYTES = 64 * 1024

# Spawn-poll window: how long we wait after Popen before asking whether
# the child has already crashed. Matches the Claude adapter pattern.
_SPAWN_POLL_SECONDS = 0.5

# Codex subprocess exit codes. Maps to fno' closed ErrorClass enum
# via map_codex_error below.
_CODEX_EXIT_USAGE_ERROR = 1
_CODEX_EXIT_SUBCOMMAND_ERROR = 2
_CODEX_EXIT_TIMEOUT = 124
_CODEX_EXIT_SIGKILL = 137
_CODEX_EXIT_SIGTERM = 143

_RETRYABLE_EXIT_CODES = (
    _CODEX_EXIT_SIGKILL,
    _CODEX_EXIT_SIGTERM,
    _CODEX_EXIT_TIMEOUT,
)

# Substrings that hint at server-side trouble when exit 2 fires without
# an explicit body-text rule match. Case-insensitive substring match.
_SERVER_SIDE_HINTS = (
    "internal error",
    "internal server error",
    "unavailable",
    "5xx",
    "server",
)


class CodexCliAdapter:
    """RuntimeAdapter for Codex CLI (``codex`` binary).

    Behavior depends on whether we are running inside a CLI agent session:

    - **In-session** (``CLAUDECODE_SESSION_ID`` or ``CODEX_SESSION_ID`` set):
      shell spawn is FORBIDDEN. ``spawn_worker`` returns a
      ``skill_dispatch_required`` sentinel.
    - **External** (no session env): spawn via ``codex exec [PROMPT]``.

    Auth modes (mirrors Claude pattern):

    - ``oauth_dir``: provider record ``auth: oauth_dir``, credentials_source: ``~/.codex``
    - ``api_key``: provider record ``auth: api_key``, env: ``{OPENAI_API_KEY: ...}``

    OAuth takes precedence in ``health()`` when both are present, matching
    Codex's own resolution preference (OAuth > env).
    """

    name: str = "codex"

    def spawn_worker(self, *, prompt: str, **kwargs) -> SpawnResult:
        """Spawn or refuse to spawn a Codex worker.

        Returns either the in-session ``skill_dispatch_required`` sentinel,
        a ``spawn_failed`` envelope when the binary is missing or the
        process crashed within ``_SPAWN_POLL_SECONDS``, or the live worker
        descriptor ``{worker_id, pid, started_at}``.
        """
        if os.environ.get("CLAUDECODE_SESSION_ID") or os.environ.get("CODEX_SESSION_ID"):
            worker_id = str(uuid.uuid4())
            return {
                "action": "skill_dispatch_required",
                "next_step": f"fno runtime register-worker {worker_id}",
                "worker_id": worker_id,
                "reason": "in-session shell spawn is forbidden; use Agent tool dispatch",
            }

        worker_id = str(uuid.uuid4())
        started_at = datetime.now(timezone.utc).isoformat()
        cmd = ["codex", "exec", prompt]

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
                "error": "codex binary not on PATH",
            }
        except OSError as exc:
            # Argument list too long, permission denied, etc.
            return {
                "action": "spawn_failed",
                "worker_id": worker_id,
                "error": f"codex spawn rejected by OS: {exc}",
            }

        # Quick health check: did the process crash within the poll window?
        time.sleep(_SPAWN_POLL_SECONDS)
        rc = proc.poll()
        if rc is not None:
            # Any exit within the poll window is suspicious - even rc==0 is a
            # zombie risk because the descriptor would point at a reaped pid.
            # Drain output for diagnostics, but never let communicate() block
            # the caller: kill on timeout and retry the drain.
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
        """Invoke a Codex subcommand list with retry on transient failures.

        Examples::

            command=["exec", "--model", "gpt-5.5", "do something"]
            command=["review", "src/foo.py"]

        Retries are attempted only when the exit code is one of the
        :data:`_RETRYABLE_EXIT_CODES` (SIGKILL / SIGTERM / timeout).
        Backoff is exponential (``2 ** attempt`` seconds, capped at 8s).

        Returns ``{"stdout": str, "stderr": str, "returncode": int}``.
        """
        cmd = ["codex"] + list(command)
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
                    "stderr": "codex binary not on PATH",
                    "returncode": 127,
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

            if last_rc not in _RETRYABLE_EXIT_CODES:
                break

            if attempt < retries:
                time.sleep(min(2 ** attempt, 8))

        return {
            "ok": False,
            "stdout": last_stdout,
            "stderr": last_stderr,
            "returncode": last_rc if last_rc is not None else -1,
        }

    def health(self) -> AdapterHealth:
        """Return health status without making a real API call.

        Checks:

        - ``codex`` binary is on PATH
        - Version >= :data:`MIN_CODEX_VERSION`
        - Auth: either ``~/.codex/auth.json`` exists OR ``OPENAI_API_KEY`` is set
        """
        details: dict = {}

        try:
            ver_result = subprocess.run(
                ["codex", "--version"],
                capture_output=True,
                text=True,
                timeout=10,
            )
        except FileNotFoundError:
            return AdapterHealth(ok=False, details={"reason": "codex binary not on PATH"})
        except subprocess.TimeoutExpired:
            return AdapterHealth(
                ok=False,
                details={"reason": "codex --version timed out after 10s"},
            )

        if ver_result.returncode != 0:
            return AdapterHealth(
                ok=False,
                details={
                    "reason": f"codex --version exited {ver_result.returncode}",
                    "stderr": ver_result.stderr[:500],
                },
            )

        version_str = ver_result.stdout.strip()
        details["version"] = version_str

        # Distinguish "unparseable version string" from "version too old".
        # Both fail the gate, but the operator-facing message is very
        # different - the first means we should not block usage based on
        # the parser, the second means upgrade.
        if not re.search(r"(\d+\.\d+\.\d+)", version_str):
            return AdapterHealth(
                ok=False,
                details={
                    **details,
                    "reason": f"could not parse codex version string: {version_str!r}",
                },
            )

        if not _version_at_least(version_str, MIN_CODEX_VERSION):
            return AdapterHealth(
                ok=False,
                details={
                    **details,
                    "reason": f"codex version too old (min {MIN_CODEX_VERSION})",
                },
            )

        codex_home = os.path.expanduser("~/.codex")
        auth_file = os.path.join(codex_home, "auth.json")
        # Existence alone passes an empty/corrupt auth file. Require non-empty.
        has_oauth = os.path.exists(auth_file) and os.path.getsize(auth_file) > 0
        has_api_key = bool(os.environ.get("OPENAI_API_KEY"))

        if not has_oauth and not has_api_key:
            return AdapterHealth(
                ok=False,
                details={
                    **details,
                    "reason": "no auth: ~/.codex/auth.json missing AND OPENAI_API_KEY unset",
                },
            )

        details["auth_status"] = "oauth" if has_oauth else "api_key"
        return AdapterHealth(ok=True, details=details)


def _version_at_least(actual: str, minimum: str) -> bool:
    """Compare SemVer-style version strings against a minimum.

    Strips any non-numeric prefix (e.g., ``"codex-cli 0.117.0"``), splits
    on dots, compares numerically. Returns False on parse failure
    (defensive: an unknown version shape is treated as "too old").
    """
    actual_match = re.search(r"(\d+\.\d+\.\d+)", actual)
    minimum_match = re.search(r"(\d+\.\d+\.\d+)", minimum)
    if not actual_match or not minimum_match:
        return False
    a_parts = [int(x) for x in actual_match.group(1).split(".")]
    m_parts = [int(x) for x in minimum_match.group(1).split(".")]
    return a_parts >= m_parts


def map_codex_error(returncode: int, stderr: str) -> NormalizedError:
    """Map a Codex subprocess outcome to a :class:`NormalizedError`.

    Order of operations (per design doc Locked Decision 4):

    1. **Universal text rules first**. Walk Plan A's :func:`classify_error`
       which knows ``rate limit``, ``too many requests``, ``quota exceeded``,
       ``capacity``, ``overloaded`` plus auth-shape phrases. Backoff-style
       matches map to :data:`ErrorClass.PROVIDER_4XX_QUOTA` and trigger swap;
       long-cooldown matches like ``no credentials`` map to
       :data:`ErrorClass.PROVIDER_4XX_AUTH`.
    2. **Codex-specific exit-code fallback** for the cases Plan A's universal
       rules don't cover (raw exit 1/2/137/143/124).
    3. **Negative returncode**. Python's :mod:`subprocess` returns
       ``-signal_number`` for signal-killed children on POSIX (so SIGKILL
       surfaces as ``-9`` from :func:`subprocess.run`, while the same
       outcome under a shell appears as ``137``). Negative values are
       normalised to their shell-style counterparts so both call paths
       classify identically.
    4. ``returncode == 0`` is defensive: success outcomes shouldn't reach
       the error mapper, but a stray caller gets ``ErrorClass.UNKNOWN``
       rather than an exception.

    The function never raises. The returned ``body_excerpt`` is truncated to
    :data:`_BODY_EXCERPT_MAX_BYTES` (256 bytes), symmetric with the
    convention enforced by :func:`normalize`.
    """
    if not isinstance(returncode, int):
        # Defensive: surface caller bugs (e.g., passing a float or str) as
        # UNKNOWN rather than raising. The function's contract is to never
        # raise from inside the error mapper.
        return NormalizedError(
            error_class=ErrorClass.UNKNOWN,
            raw_status=None,
            raw_exit_code=None,
            body_excerpt=str(stderr or "")[:_BODY_EXCERPT_MAX_BYTES],
            triggers_swap=False,
        )

    # Normalize signal-killed returncodes (-N) to their shell-style 128+N
    # equivalents so the rest of the function treats both call paths
    # uniformly. Cap signal numbers at 127; anything stranger falls
    # through to the UNKNOWN default at the bottom of the function.
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
        # backoff text rules: "rate limit", "too many requests", "quota
        # exceeded", "capacity", "overloaded". Map to provider_4xx_quota +
        # swap because the next provider in the queue might serve.
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

    # Step 2: Codex-specific exit-code fallback.
    if returncode == 0:
        return NormalizedError(
            error_class=ErrorClass.UNKNOWN,
            raw_status=None,
            raw_exit_code=0,
            body_excerpt=body_excerpt,
            triggers_swap=False,
        )

    if returncode == _CODEX_EXIT_USAGE_ERROR:
        return NormalizedError(
            error_class=ErrorClass.PARSER_ERROR,
            raw_status=None,
            raw_exit_code=returncode,
            body_excerpt=body_excerpt,
            triggers_swap=False,
        )

    if returncode == _CODEX_EXIT_SUBCOMMAND_ERROR:
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
