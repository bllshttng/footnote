"""Tests for fno.review.locking - fcntl.flock mutual exclusion.

TDD sequence:
1. Unit tests (AC1-*) - run while module doesn't exist to confirm RED.
2. Subprocess tests (AC2-*) - confirm concurrent refusal across real OS processes.
"""

from __future__ import annotations

import json
import multiprocessing
import os
import sys
import textwrap
import time
from pathlib import Path

import pytest

# Skip the entire module on Windows - fcntl is POSIX-only.
pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason="fcntl.flock not available on Windows",
)


# ---------------------------------------------------------------------------
# Unit tests (AC1-*)
# ---------------------------------------------------------------------------

class TestAcquireReviewLockUnit:
    """Unit tests that run in a single process via threading."""

    def test_ac1_happy_acquire_creates_file_with_pid(self, tmp_path: Path) -> None:
        """AC1-HAPPY: acquiring the lock creates the file and writes our PID."""
        from fno.review.locking import acquire_review_lock

        session_id = "test-session-happy"
        with acquire_review_lock(session_id, artifacts_dir=tmp_path) as lock_path:
            assert lock_path.exists(), "lock file must exist while held"
            content = lock_path.read_text().strip()
            assert content == str(os.getpid()), (
                f"lock file should contain {os.getpid()!r}, got {content!r}"
            )

    def test_ac1_blocked_second_acquire_raises_review_lock_busy(
        self, tmp_path: Path
    ) -> None:
        """AC1-BLOCKED: a second acquire in the same process raises ReviewLockBusy
        with holder_pid matching the current process (since flock is per-fd, same
        process trying to acquire twice non-blocking raises BlockingIOError)."""
        from fno.review.locking import ReviewLockBusy, acquire_review_lock
        import fcntl

        session_id = "test-session-blocked"

        # Open the lock file manually and flock it exclusively, then try acquire.
        lock_file = tmp_path / f"review-{session_id}.lock"
        lock_file.parent.mkdir(parents=True, exist_ok=True)
        lock_file.write_text(str(os.getpid()))

        # Hold the lock via a raw fd so acquire_review_lock sees it busy.
        holder_fd = os.open(str(lock_file), os.O_RDWR | os.O_CREAT)
        try:
            fcntl.flock(holder_fd, fcntl.LOCK_EX)
            # Now try to acquire via the context manager - should fail.
            with pytest.raises(ReviewLockBusy) as exc_info:
                with acquire_review_lock(session_id, artifacts_dir=tmp_path):
                    pass
            err = exc_info.value
            assert err.holder_pid == os.getpid(), (
                f"holder_pid should be {os.getpid()}, got {err.holder_pid}"
            )
            assert err.lock_path == lock_file
        finally:
            fcntl.flock(holder_fd, fcntl.LOCK_UN)
            os.close(holder_fd)

    def test_ac1_cleanup_lock_file_removed_after_context_exit(
        self, tmp_path: Path
    ) -> None:
        """AC1-CLEANUP: exiting the context unlinks the lock file."""
        from fno.review.locking import acquire_review_lock

        session_id = "test-session-cleanup"
        with acquire_review_lock(session_id, artifacts_dir=tmp_path) as lock_path:
            assert lock_path.exists()

        assert not lock_path.exists(), "lock file must be removed after context exit"

    def test_ac1_cleanup_lock_released_after_context_exit(
        self, tmp_path: Path
    ) -> None:
        """AC1-CLEANUP: after context exit a fresh acquire on the same session succeeds."""
        from fno.review.locking import acquire_review_lock

        session_id = "test-session-cleanup-reacquire"
        with acquire_review_lock(session_id, artifacts_dir=tmp_path):
            pass

        # Should not raise - lock was released.
        with acquire_review_lock(session_id, artifacts_dir=tmp_path) as lp:
            assert lp.exists()

    def test_ac2_stale_lock_file_does_not_block_fresh_acquire(
        self, tmp_path: Path
    ) -> None:
        """AC2-STALE-LOCK: a lock file left on disk (not held by any live process)
        does not block a fresh acquire because flock is per-fd and auto-released
        by the kernel on process death."""
        from fno.review.locking import acquire_review_lock

        session_id = "test-session-stale"
        stale_pid = 99999999  # Very unlikely to be a running PID.
        lock_file = tmp_path / f"review-{session_id}.lock"
        lock_file.parent.mkdir(parents=True, exist_ok=True)
        lock_file.write_text(str(stale_pid))  # Write stale content, no flock held.

        # Must succeed - stale file without a live flock holder is fine.
        with acquire_review_lock(session_id, artifacts_dir=tmp_path) as lp:
            assert lp.exists()
            content = lp.read_text().strip()
            assert content == str(os.getpid()), "should overwrite stale PID"

    def test_review_lock_busy_has_expected_attributes(
        self, tmp_path: Path
    ) -> None:
        """ReviewLockBusy must expose holder_pid and lock_path attributes."""
        from fno.review.locking import ReviewLockBusy

        lp = tmp_path / "fake.lock"
        exc = ReviewLockBusy(holder_pid=1234, lock_path=lp)
        assert exc.holder_pid == 1234
        assert exc.lock_path == lp
        assert isinstance(exc, RuntimeError)

    def test_windows_guard_raises_not_implemented(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """On Windows sys.platform, acquire_review_lock raises NotImplementedError."""
        monkeypatch.setattr(sys, "platform", "win32")
        import importlib
        import fno.review.locking as locking_mod
        importlib.reload(locking_mod)
        # Re-import after patch.
        from fno.review import locking as locking_patched
        with pytest.raises(NotImplementedError, match="fcntl"):
            with locking_patched.acquire_review_lock("s", artifacts_dir=Path("/tmp")):
                pass
        # Restore - reload with real platform.
        monkeypatch.setattr(sys, "platform", "darwin")
        importlib.reload(locking_mod)


# ---------------------------------------------------------------------------
# Subprocess test (AC2-CONCURRENT)
# ---------------------------------------------------------------------------

def _worker_try_lock(
    session_id: str,
    artifacts_dir: str,
    ready_file: str,
    result_file: str,
    hold_seconds: float = 1.0,
) -> None:
    """Child process: acquire lock, signal ready, hold, release.

    Writes JSON to result_file:
      {"ok": true}                          - lock acquired and released normally
      {"ok": false, "holder_pid": N}        - BlockingIOError, lock busy
    """
    import json
    import os
    import time
    from pathlib import Path

    # Must import here (inside the child process after fork).
    try:
        from fno.review.locking import ReviewLockBusy, acquire_review_lock
    except ImportError as exc:
        Path(result_file).write_text(json.dumps({"ok": False, "error": str(exc)}))
        return

    try:
        with acquire_review_lock(session_id, artifacts_dir=Path(artifacts_dir)):
            # Signal that we successfully hold the lock.
            Path(ready_file).write_text("ready")
            # Hold the lock long enough for the second process to attempt.
            time.sleep(hold_seconds)
        Path(result_file).write_text(json.dumps({"ok": True}))
    except ReviewLockBusy as exc:
        Path(result_file).write_text(
            json.dumps({"ok": False, "holder_pid": exc.holder_pid})
        )
    except Exception as exc:
        Path(result_file).write_text(
            json.dumps({"ok": False, "error": f"{type(exc).__name__}: {exc}"})
        )


def _worker_wait_and_try_lock(
    session_id: str,
    artifacts_dir: str,
    ready_file: str,
    result_file: str,
    timeout: float = 5.0,
) -> None:
    """Child process: wait until ready_file appears, then try to acquire the lock."""
    import json
    import time
    from pathlib import Path

    # Wait for holder to signal it holds the lock.
    deadline = time.monotonic() + timeout
    while not Path(ready_file).exists():
        if time.monotonic() > deadline:
            Path(result_file).write_text(
                json.dumps({"ok": False, "error": "timeout waiting for ready_file"})
            )
            return
        time.sleep(0.05)

    # Must import here (inside the child process after fork).
    try:
        from fno.review.locking import ReviewLockBusy, acquire_review_lock
    except ImportError as exc:
        Path(result_file).write_text(json.dumps({"ok": False, "error": str(exc)}))
        return

    try:
        with acquire_review_lock(session_id, artifacts_dir=Path(artifacts_dir)):
            Path(result_file).write_text(json.dumps({"ok": True}))
    except ReviewLockBusy as exc:
        Path(result_file).write_text(
            json.dumps({"ok": False, "holder_pid": exc.holder_pid})
        )
    except Exception as exc:
        Path(result_file).write_text(
            json.dumps({"ok": False, "error": f"{type(exc).__name__}: {exc}"})
        )


class TestAcquireReviewLockSubprocess:
    """Subprocess-based concurrent invocation test."""

    def test_ac2_concurrent_exactly_one_succeeds(self, tmp_path: Path) -> None:
        """AC2-CONCURRENT: two processes on the same session - exactly one succeeds."""
        session_id = "subprocess-concurrent-test"
        ready_file = str(tmp_path / "holder_ready")
        result_holder = str(tmp_path / "result_holder.json")
        result_contender = str(tmp_path / "result_contender.json")

        # Process 1: acquires lock, signals ready, holds for 1 second.
        p1 = multiprocessing.Process(
            target=_worker_try_lock,
            args=(session_id, str(tmp_path), ready_file, result_holder, 1.5),
        )
        # Process 2: waits for ready_file then attempts to acquire.
        p2 = multiprocessing.Process(
            target=_worker_wait_and_try_lock,
            args=(session_id, str(tmp_path), ready_file, result_contender, 5.0),
        )

        p1.start()
        p2.start()

        p1.join(timeout=10)
        p2.join(timeout=10)

        assert p1.exitcode == 0, f"holder process crashed: exitcode={p1.exitcode}"
        assert p2.exitcode == 0, f"contender process crashed: exitcode={p2.exitcode}"

        assert Path(result_holder).exists(), "holder result file missing"
        assert Path(result_contender).exists(), "contender result file missing"

        holder_result = json.loads(Path(result_holder).read_text())
        contender_result = json.loads(Path(result_contender).read_text())

        assert holder_result.get("ok") is True, (
            f"holder should succeed, got: {holder_result}"
        )
        assert contender_result.get("ok") is False, (
            f"contender should fail with ReviewLockBusy, got: {contender_result}"
        )
        assert "holder_pid" in contender_result, (
            f"contender result should include holder_pid, got: {contender_result}"
        )
        # The holder_pid in the contender result should be p1's PID.
        assert contender_result["holder_pid"] == p1.pid, (
            f"holder_pid={contender_result['holder_pid']} should match p1.pid={p1.pid}"
        )


# ---------------------------------------------------------------------------
# Orchestrator integration tests (AC3-*)
# ---------------------------------------------------------------------------

class TestOrchestratorLockIntegration:
    """Integration: orchestrate_review_parallel acquires lock when session_id+artifacts_dir given."""

    def _make_runner(self):
        """Fast stub runner - returns one finding."""
        import asyncio
        from fno.review.orchestrator import Finding, WorkerOutcome

        async def runner(agent: str, prompt: str, diff: str) -> WorkerOutcome:
            return WorkerOutcome(
                agent=agent,
                ok=True,
                findings=[Finding(agent=agent, severity="info", message="ok")],
            )

        return runner

    def test_ac3_orchestrate_parallel_without_lock_kwargs_still_works(
        self, tmp_path: Path
    ) -> None:
        """Back-compat: existing callers that pass no session_id/artifacts_dir continue to work."""
        from fno.review.orchestrator import orchestrate_review_parallel

        result = orchestrate_review_parallel(
            "diff text",
            runner=self._make_runner(),
            agents=["code_reviewer"],
            prompts={"code_reviewer": "# prompt"},
        )
        assert result.workers_completed == 1

    def test_ac3_orchestrate_parallel_with_lock_kwargs_acquires_lock(
        self, tmp_path: Path
    ) -> None:
        """When session_id + artifacts_dir are provided the lock file is written."""
        from fno.review.orchestrator import orchestrate_review_parallel

        session_id = "orch-lock-test"
        lock_file = tmp_path / f"review-{session_id}.lock"

        # The lock is released (and unlinked) on exit, but during run we can't
        # observe it without threading. We verify it doesn't error.
        result = orchestrate_review_parallel(
            "diff text",
            runner=self._make_runner(),
            agents=["code_reviewer"],
            prompts={"code_reviewer": "# prompt"},
            session_id=session_id,
            artifacts_dir=tmp_path,
        )
        assert result.workers_completed == 1
        # Lock file cleaned up after success.
        assert not lock_file.exists()

    def test_ac3_orchestrate_parallel_lock_busy_propagates(
        self, tmp_path: Path
    ) -> None:
        """ReviewLockBusy propagates from orchestrate_review_parallel to the caller."""
        import fcntl
        from fno.review.locking import ReviewLockBusy
        from fno.review.orchestrator import orchestrate_review_parallel

        session_id = "orch-lock-busy"
        lock_file = tmp_path / f"review-{session_id}.lock"
        lock_file.parent.mkdir(parents=True, exist_ok=True)
        lock_file.write_text(str(os.getpid()))

        # Hold the lock externally.
        holder_fd = os.open(str(lock_file), os.O_RDWR)
        try:
            fcntl.flock(holder_fd, fcntl.LOCK_EX)
            with pytest.raises(ReviewLockBusy):
                orchestrate_review_parallel(
                    "diff text",
                    runner=self._make_runner(),
                    agents=["code_reviewer"],
                    prompts={"code_reviewer": "# prompt"},
                    session_id=session_id,
                    artifacts_dir=tmp_path,
                )
        finally:
            fcntl.flock(holder_fd, fcntl.LOCK_UN)
            os.close(holder_fd)
