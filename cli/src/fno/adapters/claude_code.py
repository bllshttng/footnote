"""Claude Code adapter - concrete RuntimeAdapter implementation."""
from __future__ import annotations

import os
import subprocess
import uuid
from datetime import datetime, timezone

from fno.adapters._shared import create_worktree as _create_worktree
from fno.adapters.base import AdapterCallResult, AdapterHealth, SpawnResult


class ClaudeCodeAdapter:
    """RuntimeAdapter for Claude Code (claude CLI).

    Behavior depends on whether we are running inside a Claude Code session:
    - In-session (CLAUDECODE_SESSION_ID set): shell spawn is FORBIDDEN.
      spawn_worker returns a skill_dispatch_required sentinel.
    - External (no session env): spawn via `claude -p '<prompt>'`.
    """

    name: str = "claude-code"

    # ------------------------------------------------------------------
    # Primitive 1: spawn_worker
    # ------------------------------------------------------------------

    def spawn_worker(self, *, prompt: str, **kwargs) -> SpawnResult:
        """Spawn or refuse to spawn a worker agent.

        When CLAUDECODE_SESSION_ID is set we are inside a live session and
        must NOT exec a subprocess. Return a sentinel for the caller to
        dispatch via the Agent tool instead.
        """
        session_id = os.environ.get("CLAUDECODE_SESSION_ID")
        if session_id:
            worker_id = str(uuid.uuid4())
            return {
                "action": "skill_dispatch_required",
                "next_step": f"fno runtime register-worker {worker_id}",
                "worker_id": worker_id,
                "reason": "in-session shell spawn is forbidden; use Agent tool dispatch",
            }

        # External spawn: use `claude -p`. Capture stdout/stderr so early-crash
        # diagnostics are not lost to the parent's terminal (they would be
        # orphaned otherwise). Poll briefly for an immediate crash (missing
        # binary, auth expired) and surface the failure instead of reporting
        # success with a zombie pid that reap will eventually mark abandoned.
        worker_id = str(uuid.uuid4())
        started_at = datetime.now(timezone.utc).isoformat()
        cmd = ["claude", "-p", prompt]
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
                "error": "claude binary not on PATH",
            }

        # Quick health check: did the process crash within 500ms of spawn?
        import time as _time
        _time.sleep(0.5)
        rc = proc.poll()
        if rc is not None:
            # Any exit within the poll window is suspicious - even rc==0 is a
            # zombie risk because the descriptor would point at a reaped pid
            # rather than a live worker. Matches Codex / Hermes behavior so
            # all three adapters discriminate identically. Use the same
            # timeout-then-kill-then-retry pattern Codex / Hermes use so a
            # child whose stdio drain blocks does not hang the adapter
            # caller; spawn_worker MUST return a SpawnResult, not raise
            # subprocess.TimeoutExpired.
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

    # ------------------------------------------------------------------
    # Primitive 2: create_worktree
    # ------------------------------------------------------------------

    def create_worktree(self, *, name: str, base: str = "main") -> dict:
        """Create a git worktree at ``~/.fno/worktrees/{proj}-{name}/``.

        Delegates to :func:`fno.adapters._shared.create_worktree`.
        See ``_shared.create_worktree`` for return shape and error semantics.
        Symlink wiring for ``.fno/`` is handled at the runtime layer
        (``runtime/worktree.py``), not the adapter primitive.
        """
        return _create_worktree(name=name, base=base)

    # ------------------------------------------------------------------
    # Primitive 3: call_api
    # ------------------------------------------------------------------

    def call_api(self, *, command: list[str], retries: int = 3) -> AdapterCallResult:
        """Invoke a CLI command with retry logic.

        ``retries`` matches Codex / Hermes semantics: ``retries + 1`` total
        attempts (one initial attempt plus ``retries`` retries). Earlier
        revisions of this adapter used ``range(retries)`` which yielded
        only ``retries`` total attempts and was inconsistent with the
        other two adapters; that drift was caught in PR #277 review.
        """
        import time

        last_result = None
        for attempt in range(retries + 1):
            try:
                result = subprocess.run(command, capture_output=True, text=True)
            except FileNotFoundError:
                return {
                    "ok": False,
                    "stdout": "",
                    "stderr": "claude binary not on PATH",
                    "returncode": 127,
                }
            except OSError as exc:
                # Permission denied, argument list too long, etc. Surface as a
                # structured envelope instead of letting the exception cross
                # the call_api contract boundary; mirrors hermes.call_api.
                return {
                    "ok": False,
                    "stdout": "",
                    "stderr": f"claude execution failed: {exc}",
                    "returncode": -1,
                }
            last_result = result
            if result.returncode == 0:
                break
            if attempt < retries:
                time.sleep(0.5 * (attempt + 1))

        returncode = last_result.returncode if last_result else -1
        return {
            "ok": returncode == 0,
            "stdout": last_result.stdout if last_result else "",
            "stderr": last_result.stderr if last_result else "",
            "returncode": returncode,
        }

    # ------------------------------------------------------------------
    # health
    # ------------------------------------------------------------------

    def health(self) -> AdapterHealth:
        """Return health status by checking if 'claude' is on PATH."""
        import shutil

        claude_path = shutil.which("claude")
        ok = claude_path is not None
        details: dict = {
            "claude_path": claude_path or "not found",
            "in_session": bool(os.environ.get("CLAUDECODE_SESSION_ID")),
        }
        if not ok:
            details["reason"] = "claude binary not on PATH"
        return AdapterHealth(ok=ok, details=details)
